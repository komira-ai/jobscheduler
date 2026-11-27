//! The command line: flags only, never environment variables.

use std::path::PathBuf;
use std::time::Duration;

/// Help for `--server`.
const SERVER: &str = "The server, such as `https://jobs.example:7300`. Plain `http://` is accepted for a loopback host only: the bearer token never crosses a network in clear. Required";
/// Help for `--token-file`.
const TOKEN_FILE: &str =
    "A file holding the bearer token: a regular file owned by you, mode 0600 or 0400. Required";
/// Help for `--ca-cert`.
const CA_CERT: &str = "A PEM file with the certificate to trust for `https://` (the server's, or the authority that issued it); only those certificates are trusted then. Optional: without it an `https://` server is verified against the public roots bundled in the binary, never the platform's store, because the places it is found can be redirected by the environment";
/// Help for `--json`.
const JSON: &str = "Print machine output (one JSON object per line) instead of text; errors too";

/// The whole command line: the four global flags, which may stand before or after the
/// subcommand, and the job commands generated from the job service ([`crate::jobs::tree`]).
/// The flags are read by id from the parsed matches ([`crate::jobs::Connection::of`]).
pub(crate) fn command() -> clap::Command {
    clap::Command::new("js")
        .version(env!("CARGO_PKG_VERSION"))
        .about("Makes the calls of the job service")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .arg(
            clap::Arg::new("server")
                .long("server")
                .global(true)
                .value_name("URL")
                .value_parser(clap::value_parser!(String))
                .help(SERVER),
        )
        .arg(
            clap::Arg::new("token_file")
                .long("token-file")
                .global(true)
                .value_name("PATH")
                .value_parser(clap::value_parser!(PathBuf))
                .help(TOKEN_FILE),
        )
        .arg(
            clap::Arg::new("ca_cert")
                .long("ca-cert")
                .global(true)
                .value_name("PATH")
                .value_parser(clap::value_parser!(PathBuf))
                .help(CA_CERT),
        )
        .arg(
            clap::Arg::new("json")
                .long("json")
                .global(true)
                .action(clap::ArgAction::SetTrue)
                .help(JSON),
        )
        .subcommands(crate::jobs::tree())
}

/// A duration in whole seconds: `90`, `90s`, `15m` or `2h`. Zero is refused.
pub(crate) fn parse_duration(text: &str) -> Result<Duration, String> {
    let digits = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (count, unit) = text.split_at(digits);
    let factor = ["", "s", "m", "h"].iter().position(|known| *known == unit);
    let factor = factor.map(|at| [1u64, 1, 60, 3600][at]);
    let seconds = count
        .parse::<u64>()
        .ok()
        .zip(factor)
        .and_then(|(count, factor)| count.checked_mul(factor));
    match seconds {
        Some(seconds) if seconds > 0 => Ok(Duration::from_secs(seconds)),
        _ => Err(format!(
            "{text:?} is not a positive duration such as 90, 90s, 15m or 2h"
        )),
    }
}
