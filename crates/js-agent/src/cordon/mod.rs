//! `js-agent cordon`: keeps leases off this host for a window, then lets them back.
//!
//! ```text
//! js-agent cordon --flags-file <file> [--state-root <dir>] [--token-file <file>]
//!     --for <duration> [--lease-secs <n>]
//! js-agent cordon --server <url> (--host-cert <file> --host-key <file> | --state-root <dir> |
//!     --token-file <file>) --for <duration> [--lease-secs <n>]
//!     [--host-id <id> | --host-id-from-facts <file>] [--ca-cert <file>]
//! ```
//!
//! On a host the agent runs on, `--flags-file` names the agent's own flags file and the verb
//! takes the server, the certificate to trust, the host id and the host's certificate and key
//! from it, read as the agent reads them: it cordons the host the agent registers and presents
//! what the agent presents. The credential is then chosen as the agent chooses it: the host
//! certificate when the flags name one (the agent reads no `host-credential` file then, so one
//! left in the state root by an earlier registration is not sent), otherwise the state root's
//! host credential, otherwise the token file. When the server refuses the credential, standard
//! error says what was sent and for which host.
//!
//! An operator's measurement needs a node that no lease lands on for an hour or two. The
//! lease server lets a host cordon itself (`POST /v1/hosts/{id}:cordon`), with the host's own
//! credential: a cordon is a lease the caller renews, cut to an hour a request, and one that
//! is not renewed lapses. The daemon calls no such route, so this verb does: it cordons, renews
//! every third of `--lease-secs` until `--for` has passed or SIGTERM or SIGINT comes, and
//! lifts the cordon, and only a cordon it placed (the reply of the first call said
//! `already_cordoned: false`, and every reply named the host that was asked for).
//!
//! Standard output gets one line per state change, to be read by a script:
//!
//! ```text
//! cordoned host=<id> expires_in_ms=<n> leases=<n>
//! idle host=<id>
//! lifted host=<id>
//! lapsed host=<id>
//! kept host=<id> reason=already_cordoned
//! ```
//!
//! `idle` is printed once, when the host reads no lease (`lease` absent and `leases` 0), is not
//! releasing and has no mark: exactly what a drain gate checks on `GET /v1/hosts` (`lease`
//! null, `releasing` false, `mark` null). Until it is printed the host is read every
//! [`IDLE_POLL`], whatever the renewal interval is.
//!
//! The verb is answered before the agent's flags are parsed and before the daemon's signal
//! handling: it has its own, to lift on a stop. A process killed with SIGKILL leaves its
//! cordon to lapse within `--lease-secs`.
//!
//! # Exit status
//!
//! | status | meaning |
//! |---|---|
//! | 0 | the cordon was held to the end (or a stop) and lifted, or had lapsed |
//! | 1 | the server or the credential failed (the message says which); a cordon this call placed is lifted if the server can be asked |
//! | 2 | a wrong flag, or a file it names is wrong |
//! | 3 | the host was cordoned already: another holder's cordon is left in place, nothing is renewed |
//!
//! # Secrets
//!
//! The credential is read from its file by `js-client` and sent in the `Authorization` header
//! only. The host certificate of `--host-cert` and `--host-key` is presented in the TLS
//! handshake instead and no `Authorization` header is sent. Neither is in argv, in a line of
//! output or in an error.

mod args;
mod hold;

use std::ffi::OsString;
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use clap::Parser;
use js_client::ClientError;
use js_types::HostId;

use crate::cli::host_id_of;
use crate::own_host::{HttpLease, Lease, RequestLimit};

use crate::{
    CommandRunner, FileReader, Login, Platform, Sleeper, SystemCommands, SystemFiles,
    SystemSleeper, Wake, exit, host_id, machine_id,
};
pub use args::{Args, parse_window};
pub(crate) use hold::CREDENTIAL_CODES;
use hold::CREDENTIAL_ERROR;
pub use hold::{IDLE_POLL, Outcome, Plan, hold};

/// The first argument that selects the verb.
pub const VERB: &str = "cordon";

/// The exit status when the host was cordoned already: another holder's cordon.
pub const EXIT_OTHER_HOLDER: u8 = 3;

/// Time and the stop request. The program's is [`SystemPace`]; a test supplies a clock it
/// moves by hand.
pub trait Pace {
    /// The time that has passed since some fixed moment, on a clock that does not go back.
    fn now(&self) -> Duration;

    /// Waits `span`, or until a stop request arrives, whichever is first. A stop request that
    /// arrived earlier ends the wait at once.
    fn wait(&mut self, span: Duration) -> Wake;
}

/// [`Pace`] on the machine: the monotonic clock, and the signal thread of the daemon.
pub struct SystemPace {
    began: Instant,
    sleeper: SystemSleeper,
}

impl SystemPace {
    /// Time from now on, stopped by `sleeper`'s SIGTERM and SIGINT.
    #[must_use]
    pub fn new(sleeper: SystemSleeper) -> Self {
        Self {
            began: Instant::now(),
            sleeper,
        }
    }
}

impl Pace for SystemPace {
    fn now(&self) -> Duration {
        self.began.elapsed()
    }

    fn wait(&mut self, span: Duration) -> Wake {
        self.sleeper.sleep(span)
    }
}

/// What the client is connected to.
#[derive(Clone, Copy, Debug)]
pub struct Connection<'a> {
    /// `--server`.
    pub server: &'a str,
    /// How the client proves who it is: the credential file or the host certificate.
    pub login: &'a Login,
    /// `--ca-cert`.
    pub ca_cert: Option<&'a Path>,
    /// The host the routes are called for.
    pub host: &'a str,
}

/// Connects to the lease server as the host `at` names.
pub type Connect<'a> = dyn Fn(&Connection<'_>) -> Result<Box<dyn Lease>, ClientError> + 'a;

/// What the verb takes from the machine it runs on.
pub struct Host<'a> {
    /// The platform whose machine id names the default host.
    pub platform: Platform,
    /// Starts the programs the machine id is read with.
    pub commands: &'a dyn CommandRunner,
    /// Reads the files the machine id is read from.
    pub files: &'a dyn FileReader,
    /// Connects to the lease server.
    pub connect: &'a Connect<'a>,
    /// Time and the stop request.
    pub pace: &'a mut dyn Pace,
}

/// Why the window did not begin.
enum Early {
    /// `--help`: the text for standard output.
    Help(String),
    /// A wrong flag or file; exit status 2.
    Usage(String),
}

/// What the verb holds once the flags are read and the server is connected.
struct Ready {
    args: Args,
    /// The host to cordon.
    id: String,
    lease: Box<dyn Lease>,
    /// What the verb sends as its credential, for a person.
    said: String,
}

impl Ready {
    /// What standard error is told after the server refused the credential: what was sent and
    /// for which host, and, when the verb's own flags chose it, how to choose what the agent
    /// on this host sends.
    fn refused_note(&self) -> String {
        let sent = format!("it sent {} for host {}", self.said, self.id);
        match self.args.flags_file {
            Some(_) => sent,
            None => format!(
                "{sent}; to cordon with what the agent on this host presents, give the agent's \
                 flags file with --flags-file (such as /etc/js-agent/flags)"
            ),
        }
    }
}

/// Reads the flags and connects.
fn begin(args: &[OsString], host: &Host<'_>) -> Result<Ready, Early> {
    let parsed = match Args::try_parse_from(args.iter().skip(1)) {
        Ok(parsed) => parsed,
        Err(e) if e.use_stderr() => return Err(Early::Usage(e.to_string())),
        Err(help) => return Err(Early::Help(help.to_string())),
    };
    let euid = rustix::process::geteuid().as_raw();
    let read = |path: &Path| host.files.facts(path);
    let settings = args::settings(&parsed, euid, &read).map_err(Early::Usage)?;
    let named = host_id_of(
        settings.host_id.as_deref(),
        settings.host_id_from_facts.as_deref(),
        euid,
        &read,
    )
    .map_err(|why| Early::Usage(why.to_string()))?;
    let id = match named {
        Some(id) => id,
        None => machine_id(&host.platform, host.commands, host.files)
            .map(|machine| host_id(&machine))
            .map_err(|why| {
                Early::Usage(format!(
                    "cannot read the machine id ({why}); pass --host-id"
                ))
            })?,
    };
    HostId::new(&id).map_err(|why| Early::Usage(format!("--host-id: {why}")))?;
    let chosen = args::login(&settings).map_err(Early::Usage)?;
    let lease = (host.connect)(&Connection {
        server: &settings.server,
        login: &chosen.login,
        ca_cert: settings.ca_cert.as_deref(),
        host: &id,
    })
    .map_err(|why| Early::Usage(why.to_string()))?;
    Ok(Ready {
        args: parsed,
        id,
        lease,
        said: chosen.said,
    })
}

/// Runs the verb on `args` (the program name and the verb first): the lines of the window go
/// to `out`, the words for a person to `err`.
pub fn run(
    args: &[OsString],
    out: &mut dyn Write,
    err: &mut dyn Write,
    host: &mut Host<'_>,
) -> ExitCode {
    let ready = match begin(args, host) {
        Ok(ready) => ready,
        Err(Early::Help(text)) => {
            let _ = write!(out, "{text}");
            return ExitCode::from(exit::OK);
        }
        Err(Early::Usage(text)) => {
            let _ = writeln!(err, "js-agent cordon: {text}");
            return ExitCode::from(exit::USAGE);
        }
    };
    let plan = Plan {
        host: &ready.id,
        lease_secs: ready.args.lease_secs,
        window: ready.args.window,
    };
    match hold(&*ready.lease, &mut *host.pace, &plan, out) {
        Outcome::Done => ExitCode::from(exit::OK),
        Outcome::OtherHolder => {
            let _ = writeln!(
                err,
                "js-agent cordon: host {} is cordoned by another holder; it is left in place",
                ready.id
            );
            ExitCode::from(EXIT_OTHER_HOLDER)
        }
        Outcome::Failed(text) => {
            let _ = writeln!(err, "js-agent cordon: {text}");
            if text.starts_with(CREDENTIAL_ERROR) {
                let _ = writeln!(err, "js-agent cordon: {}", ready.refused_note());
            }
            ExitCode::from(exit::FAILED)
        }
    }
}

/// The verb on this machine: the real server connection, the monotonic clock and the signals
/// of the process. `install` makes the signal handling, so a test can make it fail.
pub fn run_system(
    install: &dyn Fn() -> std::io::Result<SystemSleeper>,
    args: &[OsString],
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let sleeper = match install() {
        Ok(sleeper) => sleeper,
        Err(why) => {
            let _ = writeln!(
                err,
                "js-agent cordon: cannot install signal handlers: {why}"
            );
            return ExitCode::from(exit::FAILED);
        }
    };
    let mut pace = SystemPace::new(sleeper);
    let connect = |at: &Connection<'_>| {
        HttpLease::connect(
            at.server,
            at.login,
            at.ca_cert,
            at.host,
            RequestLimit::DEFAULT,
        )
        .map(|lease| Box::new(lease) as Box<dyn Lease>)
    };
    let mut host = Host {
        platform: Platform::current(),
        commands: &SystemCommands::default(),
        files: &SystemFiles,
        connect: &connect,
        pace: &mut pace,
    };
    run(args, out, err, &mut host)
}
