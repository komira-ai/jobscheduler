//! `js-agent cordon`: keeps leases off this host for a window, then lets them back.
//!
//! ```text
//! js-agent cordon --server <url> (--state-root <dir> | --token-file <file>) --for <duration>
//!     [--lease-secs <n>] [--host-id <id>] [--ca-cert <file>]
//! ```
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
//! only. It is not in argv, in a line of output or in an error.

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

use crate::own_host::{HttpLease, Lease};

use crate::{
    CommandRunner, FileReader, Platform, Sleeper, SystemCommands, SystemFiles, SystemSleeper, Wake,
    exit, host_id, machine_id,
};
pub use args::{Args, parse_window};
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
    /// The file the credential is read from.
    pub credential: &'a Path,
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

/// Reads the flags and connects.
fn begin(args: &[OsString], host: &Host<'_>) -> Result<(Args, String, Box<dyn Lease>), Early> {
    let parsed = match Args::try_parse_from(args.iter().skip(1)) {
        Ok(parsed) => parsed,
        Err(e) if e.use_stderr() => return Err(Early::Usage(e.to_string())),
        Err(help) => return Err(Early::Help(help.to_string())),
    };
    let id = match &parsed.host_id {
        Some(id) => id.clone(),
        None => machine_id(&host.platform, host.commands, host.files)
            .map(|machine| host_id(&machine))
            .map_err(|why| {
                Early::Usage(format!(
                    "cannot read the machine id ({why}); pass --host-id"
                ))
            })?,
    };
    HostId::new(&id).map_err(|why| Early::Usage(format!("--host-id: {why}")))?;
    let credential = args::credential_file(&parsed).map_err(Early::Usage)?;
    let lease = (host.connect)(&Connection {
        server: &parsed.server,
        credential: &credential,
        ca_cert: parsed.ca_cert.as_deref(),
        host: &id,
    })
    .map_err(|why| Early::Usage(why.to_string()))?;
    Ok((parsed, id, lease))
}

/// Runs the verb on `args` (the program name and the verb first): the lines of the window go
/// to `out`, the words for a person to `err`.
pub fn run(
    args: &[OsString],
    out: &mut dyn Write,
    err: &mut dyn Write,
    host: &mut Host<'_>,
) -> ExitCode {
    let (parsed, id, lease) = match begin(args, host) {
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
        host: &id,
        lease_secs: parsed.lease_secs,
        window: parsed.window,
    };
    match hold(&*lease, &mut *host.pace, &plan, out) {
        Outcome::Done => ExitCode::from(exit::OK),
        Outcome::OtherHolder => {
            let _ = writeln!(
                err,
                "js-agent cordon: host {id} is cordoned by another holder; it is left in place"
            );
            ExitCode::from(EXIT_OTHER_HOLDER)
        }
        Outcome::Failed(text) => {
            let _ = writeln!(err, "js-agent cordon: {text}");
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
        HttpLease::connect(at.server, at.credential, at.ca_cert, at.host)
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
