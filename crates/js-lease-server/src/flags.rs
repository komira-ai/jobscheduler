//! Which flags a command line gave, for the two rules that depend on it: a flag beside
//! `--config` is refused, and the flags of agent registration are refused without the
//! pool that turns it on.

use std::ffi::OsString;

use clap::parser::ValueSource;
use clap::{ArgMatches, Command, CommandFactory, FromArgMatches};

use crate::config::Cli;
use crate::error::StartupError;

/// The flags that mean something only beside `--agent-pool-config`.
const NEED_AGENT_POOL: [&str; 3] = [
    "--agent-hello-burst",
    "--agent-hello-interval",
    "--require-host-credential",
];

impl Cli {
    /// Parses `args` (the program name first) as [`clap::Parser::try_parse_from`] does,
    /// and records which flags the command line gave besides `--config`, so that
    /// [`Config::from_cli`](crate::Config::from_cli) can refuse them beside it. A flag
    /// that kept its default is not recorded; one given with its default value is.
    ///
    /// # Errors
    ///
    /// The usage error `clap` finds.
    pub fn try_parse_from<I, T>(args: I) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        let matches = Self::command().try_get_matches_from(args)?;
        let mut cli = Self::from_arg_matches(&matches)?;
        cli.given = Some(given(&Self::command(), &matches));
        Ok(cli)
    }
}

/// The flags of `command` that `matches` got from the command line, `--config` aside, in
/// the order they were written.
fn given(command: &Command, matches: &ArgMatches) -> Vec<String> {
    let mut found: Vec<(usize, String)> = command
        .get_arguments()
        .filter(|arg| arg.get_id().as_str() != "config")
        .filter(|arg| {
            let typed = matches.value_source(arg.get_id().as_str());
            typed == Some(ValueSource::CommandLine)
        })
        .filter_map(|arg| {
            let at = matches.index_of(arg.get_id().as_str());
            arg.get_long()
                .zip(at)
                .map(|(long, at)| (at, format!("--{long}")))
        })
        .collect();
    found.sort();
    found.into_iter().map(|(_, flag)| flag).collect()
}

/// The refusal of `flag` given beside `--config`: it names the flag and where the file
/// takes its place.
pub(crate) fn beside_config(flag: &str) -> StartupError {
    let instead = match flag {
        "--token-file" => "set server.client_token_file".to_owned(),
        "--pool-config" => "add a [[pool]] with provider kind static".to_owned(),
        "--agent-pool-config" => "add a [[pool]] with an elastic provider".to_owned(),
        "--agent-token-file" => "set join_token_file of the [[pool]]".to_owned(),
        "--usage-db" | "--usage-off" | "--profile-store-bytes" => {
            "the file has no usage keys; usage is on, beside the lease file".to_owned()
        }
        other => format!(
            "set server.{}",
            other.trim_start_matches("--").replace('-', "_")
        ),
    };
    StartupError::at(flag, format!("not allowed with --config; {instead}"))
}

/// The flags `cli` recorded, or the refusal of a `Cli` parsed any way but
/// [`Cli::try_parse_from`], which would let a flag beside `--config` pass unseen.
pub(crate) fn recorded(cli: &Cli) -> Result<&[String], StartupError> {
    match &cli.given {
        Some(given) => Ok(given),
        None => Err(StartupError::new(
            "--config",
            "the command line was parsed without recording its flags; parse it with Cli::try_parse_from",
        )),
    }
}

/// Refuses an agent registration flag given without `--agent-pool-config`, which alone
/// turns registration on: the flag would do nothing.
pub(crate) fn need_agent_pool(cli: &Cli) -> Result<(), StartupError> {
    let stray = cli
        .given
        .iter()
        .flatten()
        .find(|flag| NEED_AGENT_POOL.contains(&flag.as_str()));
    match (stray, &cli.agent_pool_config) {
        (Some(flag), None) => Err(StartupError::at(flag, "requires --agent-pool-config")),
        _ => Ok(()),
    }
}
