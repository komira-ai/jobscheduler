//! The command line of the verb: `js-agent cordon --server URL ... --for 2h`.
//!
//! Flags only: no value comes from the environment. The connection is named by the verb's own
//! flags or by the agent's flags file ([`settings`]), and the verb is identified as the agent is, by the host's
//! certificate and key (see [`login`]).

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Parser;
use js_client::HostIdentity;
use js_types::CordonHolder;

use crate::Login;
use crate::cli::parse_agent_flags;
use crate::flags_file::{self, FileFacts};
use crate::own_host::MAX_CORDON_SECS;

/// What `--help` says after the flags.
const AFTER_HELP: &str = "\
The verb cordons this host on the lease server (no new lease is placed on it) as a named \
holder (--holder, by default a name made for this run and printed on the cordoned line), \
renews the cordon every third of --lease-secs until --for has passed, and lifts it. Every \
call names the holder, so the server says whose the cordon is: the verb extends and lifts \
its own and never another holder's. It prints one line per state change on standard output:

  cordoned host=<id> holder=<h> expires_in_ms=<n> leases=<n>
                                                    the cordon was placed (or taken over, or
                                                    placed again after a lapse of ours)
  idle host=<id>                                    once: no lease, not releasing, no mark
  lifted host=<id>                                  the cordon was lifted at the end or on a stop
  lapsed host=<id>                                  the cordon had lapsed already, nothing to lift
  kept host=<id> reason=already_cordoned holder=<h|-> expires_in_ms=<n>
                                                    another holder's cordon was found; it is not
                                                    extended and not lifted
  kept host=<id> reason=taken_over holder=<h|->     our cordon lapsed and another holder's took
                                                    its place, or it was taken over; not lifted

With --take-over the cordon in force is taken over whoever holds it, and held and lifted as the \
verb's own. A cordon found is not extended: one whose holder died lapses within its last \
--lease-secs, and the next run then places its own cordon; to go on sooner, take it over, or \
lift it with `js host uncordon --host-id <id>`. The verb needs a lease server of this release \
(an earlier one refuses the holder with invalid-cordon).

On a host the agent runs on, give the agent's own flags file (--flags-file \
/etc/js-agent/flags): the server, the host id and the host certificate are then the agent's. \
When the server refuses the certificate, standard error says what was sent and for which host.

SIGTERM, SIGINT and SIGHUP (the end of an ssh session) end the window early and lift the cordon. A process that is killed with \
SIGKILL (or loses the machine) lifts nothing: the cordon is a lease that is renewed, and it \
lapses by itself within --lease-secs of the last renewal.

Exit status: 0 held and lifted (or lapsed), 1 a server or credential error, 2 a wrong flag \
or file, 3 another holder's cordon was found, at the start or later (it is left in place).";

/// The flags of `js-agent cordon`.
#[derive(Debug, Parser)]
#[command(
    name = "cordon",
    about = "Keeps leases off this host for a window, then lets them back",
    after_help = AFTER_HELP
)]
pub struct Args {
    /// The agent's own flags file (the file `js-agent --flags-file` reads, such as
    /// `/etc/js-agent/flags`): the lease server, the certificate to trust, the host id and the
    /// host's certificate and key are taken from it, so the verb cordons the host the agent
    /// registers and presents what the agent presents. The flags the file names may not be
    /// given beside it.
    #[arg(
        long,
        value_name = "PATH",
        conflicts_with_all = [
            "server",
            "ca_cert",
            "host_id",
            "host_id_from_facts",
            "host_cert",
            "host_key",
        ]
    )]
    pub flags_file: Option<PathBuf>,
    /// The lease server, such as `https://leases.example:7300`. Plain `http://` is accepted
    /// for a loopback host only. Required unless `--flags-file` names it.
    #[arg(long, value_name = "URL", required_unless_present = "flags_file")]
    pub server: Option<String>,
    /// Accepted and ignored, so a command line written for an earlier release still runs:
    /// the verb is identified by the host's certificate and key alone.
    #[arg(long, value_name = "DIR", hide = true)]
    pub state_root: Option<PathBuf>,
    /// Accepted and ignored, as `--state-root` is.
    #[arg(long, value_name = "PATH", hide = true)]
    pub token_file: Option<PathBuf>,
    /// The host's own certificate, the file `--host-cert` of the agent's flags file names.
    /// With `--host-key` it is presented to the lease server, as the agent presents it: it is
    /// the only identity the verb has, so it is required here or in `--flags-file`. Both flags
    /// or neither.
    #[arg(long, value_name = "PATH", requires = "host_key")]
    pub host_cert: Option<PathBuf>,
    /// The key of `--host-cert`, which must pass the agent's rule for it: owned by root or by
    /// the user running the verb, no access for others, not writable by its group. Both flags
    /// or neither.
    #[arg(long, value_name = "PATH", requires = "host_cert")]
    pub host_key: Option<PathBuf>,
    /// A PEM file with the certificate to trust for `https://`; only that is trusted then.
    /// Without it the public roots bundled in the program are.
    #[arg(long, value_name = "PATH")]
    pub ca_cert: Option<PathBuf>,
    /// The host to cordon. By default the id the agent registers under without `--host-id`:
    /// `h-` and 32 hex digits of a salted hash of the machine id.
    #[arg(long, value_name = "ID", conflicts_with = "host_id_from_facts")]
    pub host_id: Option<String>,
    /// The host to cordon is the `host_id` of this facts file of hostkeeper's daemon
    /// (`/var/lib/hostkeeper/facts.json`), for an agent that registers with the same flag.
    #[arg(long, value_name = "PATH")]
    pub host_id_from_facts: Option<PathBuf>,
    /// How long to keep the host cordoned, such as `90m`, `2h` or `1h30m` (units `s`, `m`,
    /// `h`, `d`).
    #[arg(long = "for", value_name = "DURATION", value_parser = parse_window)]
    pub window: Duration,
    /// The length of each cordon request in seconds, from 1 to 3600 (the server cuts a longer
    /// one). The cordon is renewed every third of it.
    #[arg(
        long,
        value_name = "N",
        default_value_t = 300,
        value_parser = clap::value_parser!(u64).range(1..=MAX_CORDON_SECS)
    )]
    pub lease_secs: u64,
    /// Who holds the cordon: 1 to 63 characters of `[a-z0-9._-]`, sent with every request.
    /// The cordon is renewed and lifted for this holder only, and a cordon of another holder
    /// is left as it is. By default a name made for this run, which the `cordoned` line
    /// prints. Give a stable name to tell one's own cordon from a requester's that died.
    #[arg(long, value_name = "NAME", value_parser = parse_holder)]
    pub holder: Option<CordonHolder>,
    /// Takes the cordon in force over, whoever holds it, and holds and lifts it as the
    /// verb's own. Without it a cordon found is another holder's and is left as it is.
    #[arg(long)]
    pub take_over: bool,
}

/// A holder name, as `--holder` takes it.
///
/// # Errors
///
/// Text that is empty, longer than 63 characters or has a character outside `[a-z0-9._-]`.
pub fn parse_holder(text: &str) -> Result<CordonHolder, String> {
    CordonHolder::new(text).map_err(|why| why.to_string())
}

/// A length of time as `<n><unit>` terms, such as `90m` or `1h30m`.
///
/// # Errors
///
/// Text that is not one or more terms of digits and a unit (`s`, `m`, `h`, `d`), whose
/// total is zero or too large to count in seconds.
pub fn parse_window(text: &str) -> Result<Duration, String> {
    let bad = || format!("{text:?} is not a duration such as 90m, 2h or 1h30m (units s, m, h, d)");
    let mut total = Some(0_u64);
    let mut number = String::new();
    for c in text.chars() {
        let unit = match c {
            '0'..='9' => {
                number.push(c);
                continue;
            }
            's' => 1,
            'm' => 60,
            'h' => 3600,
            'd' => 86400,
            _ => return Err(bad()),
        };
        let seconds = number.parse::<u64>().ok().and_then(|n| n.checked_mul(unit));
        total = total
            .zip(seconds)
            .and_then(|(sum, term)| sum.checked_add(term));
        number.clear();
    }
    match (total, number.is_empty()) {
        (Some(secs @ 1..), true) => Ok(Duration::from_secs(secs)),
        _ => Err(bad()),
    }
}

/// Where the verb connects and as whom: the flags of the agent's flags file when
/// `--flags-file` is given, otherwise the verb's own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Settings {
    /// The lease server.
    pub(super) server: String,
    /// The certificate to trust for `https://`.
    pub(super) ca_cert: Option<PathBuf>,
    /// `--host-id`.
    pub(super) host_id: Option<String>,
    /// `--host-id-from-facts`.
    pub(super) host_id_from_facts: Option<PathBuf>,
    /// `--host-cert`.
    pub(super) host_cert: Option<PathBuf>,
    /// `--host-key`.
    pub(super) host_key: Option<PathBuf>,
}

/// The settings `args` name. With `--flags-file` the file is read as the agent reads it (by
/// the user `euid`, with the facts of a file from `read`).
///
/// # Errors
///
/// The flags file cannot be used or does not hold the agent's flags; the text names the file or
/// the flag.
pub(super) fn settings(
    args: &Args,
    euid: u32,
    read: &dyn Fn(&Path) -> io::Result<FileFacts>,
) -> Result<Settings, String> {
    let Some(file) = &args.flags_file else {
        return Ok(Settings {
            // clap requires `--server` when there is no `--flags-file`.
            server: args.server.clone().unwrap_or_default(),
            ca_cert: args.ca_cert.clone(),
            host_id: args.host_id.clone(),
            host_id_from_facts: args.host_id_from_facts.clone(),
            host_cert: args.host_cert.clone(),
            host_key: args.host_key.clone(),
        });
    };
    let words: Vec<OsString> = vec![
        "js-agent".into(),
        flags_file::FLAG.into(),
        file.as_os_str().to_owned(),
    ];
    let cli = parse_agent_flags(words, euid, read)?;
    Ok(Settings {
        server: cli.server,
        ca_cert: cli.ca_cert,
        host_id: cli.host.host_id,
        host_id_from_facts: cli.host.host_id_from_facts,
        host_cert: cli.host_cert,
        host_key: cli.host_key,
    })
}

/// How the verb proves who it is, and the words that tell a person which it was.
#[derive(Debug)]
pub(super) struct Chosen {
    /// The login.
    pub(super) login: Login,
    /// What it sends, such as `the host certificate /var/lib/hostkeeper/identity/host.crt`:
    /// a path and never a byte of a file.
    pub(super) said: String,
}

/// How the verb proves who it is: the host's certificate and key, which is what the agent
/// presents.
///
/// # Errors
///
/// There is no host certificate and key, here or in `--flags-file`.
pub(super) fn login(settings: &Settings) -> Result<Chosen, String> {
    match (&settings.host_cert, &settings.host_key) {
        (Some(cert), Some(key)) => Ok(Chosen {
            login: Login::HostCertificate(HostIdentity::new(cert, key)),
            said: format!("the host certificate {}", cert.display()),
        }),
        _ => Err(
            "--host-cert and --host-key: required, here or in the --flags-file that names them"
                .to_owned(),
        ),
    }
}
