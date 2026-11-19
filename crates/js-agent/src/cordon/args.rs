//! The command line of the verb: `js-agent cordon --server URL ... --for 2h`.
//!
//! Flags only: no value comes from the environment. The connection is named by the verb's own
//! flags or by the agent's flags file ([`settings`]), and the credential rules are the agent's
//! (see [`login`]).

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Parser;
use js_client::HostIdentity;

use crate::cli::parse_agent_flags;
use crate::flags_file::{self, FileFacts};
use crate::own_host::MAX_CORDON_SECS;
use crate::{CredentialFile, Login};

/// What `--help` says after the flags.
const AFTER_HELP: &str = "\
The verb cordons this host on the lease server (no new lease is placed on it), renews the \
cordon every third of --lease-secs until --for has passed, and lifts it. It prints one line \
per state change on standard output:

  cordoned host=<id> expires_in_ms=<n> leases=<n>   the cordon was placed
  idle host=<id>                                    once: no lease, not releasing, no mark
  lifted host=<id>                                  the cordon was lifted at the end or on a stop
  lapsed host=<id>                                  the cordon had lapsed already, nothing to lift
  kept host=<id> reason=already_cordoned            another holder's cordon was found; not lifted
  kept host=<id> reason=taken_after_lapse          ours lapsed (a stalled process) and another
                                                    holder's took its place; not lifted

On a host the agent runs on, give the agent's own flags file and state root (--flags-file \
/etc/js-agent/flags --state-root /var/lib/js-agent/state): the server, the host id and the \
credential are then the agent's. When the server refuses the credential, standard error says \
what was sent and for which host.

SIGTERM and SIGINT end the window early and lift the cordon. A process that is killed with \
SIGKILL (or loses the machine) lifts nothing: the cordon is a lease that is renewed, and it \
lapses by itself within --lease-secs of the last renewal.

Exit status: 0 held and lifted (or lapsed), 1 a server or credential error, 2 a wrong flag \
or file, 3 another holder's cordon was found (it is left in place).";

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
    /// registers and presents what the agent presents. `--state-root` and `--token-file` may be
    /// given beside it, as the agent's unit gives them beside the file; the flags the file
    /// names may not.
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
    /// The agent's state root: the host credential kept there is used. The cordon routes
    /// serve the host of the credential only, so the pool's join token is refused. Not used
    /// when `--host-cert` and `--host-key` are given, here or in `--flags-file`.
    #[arg(long, value_name = "DIR")]
    pub state_root: Option<PathBuf>,
    /// A file holding the host's one-time token, in a regular file owned by the user running
    /// the verb, mode 0600 or 0400. Used when there is no `--host-cert` and `--state-root`
    /// holds no credential; they win, as they do for the agent.
    #[arg(long, value_name = "PATH")]
    pub token_file: Option<PathBuf>,
    /// The host's own certificate, the file `--host-cert` of the agent's flags file names.
    /// With `--host-key` it is presented to the lease server in place of any credential, as
    /// the agent presents it, and no token is sent. Both flags or neither.
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
    /// `--state-root`.
    pub(super) state_root: Option<PathBuf>,
    /// `--token-file`.
    pub(super) token_file: Option<PathBuf>,
}

/// The settings `args` name. With `--flags-file` the file is read as the agent reads it (by
/// the user `euid`, with the facts of a file from `read`), followed by the verb's
/// `--state-root` and `--token-file`, so a flag the file names too is refused as the agent
/// refuses it.
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
            state_root: args.state_root.clone(),
            token_file: args.token_file.clone(),
        });
    };
    let mut words: Vec<OsString> = vec![
        "js-agent".into(),
        flags_file::FLAG.into(),
        file.as_os_str().to_owned(),
    ];
    let beside = [
        ("--state-root", &args.state_root),
        ("--token-file", &args.token_file),
    ];
    for (flag, value) in beside {
        if let Some(value) = value {
            words.extend([flag.into(), value.as_os_str().to_owned()]);
        }
    }
    let cli = parse_agent_flags(words, euid, read)?;
    Ok(Settings {
        server: cli.server,
        ca_cert: cli.ca_cert,
        host_id: cli.host.host_id,
        host_id_from_facts: cli.host.host_id_from_facts,
        host_cert: cli.host_cert,
        host_key: cli.host_key,
        state_root: cli.state_root,
        token_file: cli.token_file,
    })
}

/// How the verb proves who it is, and the words that tell a person which it was.
#[derive(Debug)]
pub(super) struct Chosen {
    /// The login.
    pub(super) login: Login,
    /// What it sends, such as `the host credential in /var/lib/js-agent/state/host-credential`:
    /// a path and never a byte of a file.
    pub(super) said: String,
}

/// How the verb proves who it is: the host's certificate and key when `--host-cert` and
/// `--host-key` are given, which is what the agent presents then; otherwise the host
/// credential of `--state-root` when it holds one, which is what the agent sends once it has
/// one; otherwise `--token-file`.
///
/// # Errors
///
/// The credential file is there and cannot be used (the text names the file and never holds a
/// byte of its content), or there is no host certificate, no credential and no token file.
pub(super) fn login(settings: &Settings) -> Result<Chosen, String> {
    if let (Some(cert), Some(key)) = (&settings.host_cert, &settings.host_key) {
        return Ok(Chosen {
            login: Login::HostCertificate(HostIdentity::new(cert, key)),
            said: format!("the host certificate {}", cert.display()),
        });
    }
    let kept = match &settings.state_root {
        Some(root) => {
            let file = CredentialFile::in_dir(root);
            file.load()?.map(|_| file.path())
        }
        None => None,
    };
    let said = |what: &str, path: &Path| format!("{what} {}", path.display());
    let chosen = match (kept, &settings.token_file) {
        (Some(path), _) => Chosen {
            said: said("the host credential in", &path),
            login: Login::Token(path),
        },
        (None, Some(path)) => Chosen {
            said: said("the token file", path),
            login: Login::Token(path.clone()),
        },
        (None, None) => {
            return Err(
                "--token-file: required unless --state-root holds a host credential".to_owned(),
            );
        }
    };
    Ok(chosen)
}
