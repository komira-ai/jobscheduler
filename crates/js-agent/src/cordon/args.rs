//! The command line of the verb: `js-agent cordon --server URL ... --for 2h`.
//!
//! Flags only: no value comes from the environment. The credential rules are the agent's
//! (see [`credential_file`]).

use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;

use crate::CredentialFile;
use crate::own_host::MAX_CORDON_SECS;

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
    /// The lease server, such as `https://leases.example:7300`. Plain `http://` is accepted
    /// for a loopback host only.
    #[arg(long, value_name = "URL")]
    pub server: String,
    /// The agent's state root: the host credential kept there is used. The cordon routes
    /// serve the host of the credential only, so the pool's join token is refused.
    #[arg(long, value_name = "DIR")]
    pub state_root: Option<PathBuf>,
    /// A file holding the host's one-time token, in a regular file owned by the user running
    /// the verb, mode 0600 or 0400. Used when `--state-root` holds no credential; a
    /// credential there wins, as it does for the agent.
    #[arg(long, value_name = "PATH")]
    pub token_file: Option<PathBuf>,
    /// A PEM file with the certificate to trust for `https://`; only that is trusted then.
    /// Without it the public roots bundled in the program are.
    #[arg(long, value_name = "PATH")]
    pub ca_cert: Option<PathBuf>,
    /// The host to cordon. By default the id the agent registers under: `h-` and 32 hex
    /// digits of a salted hash of the machine id.
    #[arg(long, value_name = "ID")]
    pub host_id: Option<String>,
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

/// The file the bearer is read from: the host credential of `--state-root` when it holds
/// one, which is what the agent sends once it has one, and otherwise `--token-file`.
///
/// # Errors
///
/// The credential file is there and cannot be used (the text names the file and never holds
/// a byte of its content), or there is neither a credential nor a token file.
pub fn credential_file(args: &Args) -> Result<PathBuf, String> {
    let kept = match &args.state_root {
        Some(root) => {
            let file = CredentialFile::in_dir(root);
            file.load()?.map(|_| file.path())
        }
        None => None,
    };
    kept.or_else(|| args.token_file.clone()).ok_or_else(|| {
        "--token-file: required unless --state-root holds a host credential".to_owned()
    })
}
