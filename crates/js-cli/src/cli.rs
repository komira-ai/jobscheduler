//! The command line: flags only, never environment variables.

use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, CommandFactory, Parser, Subcommand};
use serde_json::{Value, json};

use crate::failure::Failure;

/// The command line of `js`. The four global flags may stand before or after the
/// subcommand.
#[derive(Debug, Parser)]
#[command(
    name = "js",
    version,
    about = "Makes the calls of the job service and mints the join tokens that admit agents"
)]
pub struct Cli {
    /// The server, such as `https://jobs.example:7300`. Plain `http://` is
    /// accepted for a loopback host only: the bearer token never crosses a network in
    /// clear. Required.
    #[arg(long, global = true, value_name = "URL")]
    pub(crate) server: Option<String>,
    /// A file holding the bearer token: a regular file owned by you, mode 0600 or 0400.
    /// Required.
    #[arg(long, global = true, value_name = "PATH")]
    pub(crate) token_file: Option<PathBuf>,
    /// A PEM file with the certificate to trust for `https://` (the server's, or the
    /// authority that issued it); only those certificates are trusted then. Optional:
    /// without it an `https://` server is verified against the public roots bundled in the
    /// binary, never the platform's store, because the places it is found can be redirected
    /// by the environment.
    #[arg(long, global = true, value_name = "PATH")]
    pub(crate) ca_cert: Option<PathBuf>,
    /// Print machine output (one JSON object per line) instead of text; errors too.
    #[arg(long, global = true)]
    pub(crate) json: bool,
    #[command(subcommand)]
    pub(crate) command: Command,
}

/// The whole command line: the `join-token` commands of [`Cli`] and the job commands generated
/// from the job service ([`crate::jobs::commands`]).
pub(crate) fn command() -> clap::Command {
    Cli::command().subcommands(crate::jobs::tree())
}

impl Cli {
    /// The `--server` value, or the usage failure for its absence.
    pub(crate) fn need_server(&self) -> Result<&str, Failure> {
        let given = self.server.as_deref();
        given.ok_or_else(|| Failure::flag("--server", "is required"))
    }

    /// The `--token-file` value, or the usage failure for its absence.
    pub(crate) fn need_token_file(&self) -> Result<&std::path::Path, Failure> {
        let given = self.token_file.as_deref();
        given.ok_or_else(|| Failure::flag("--token-file", "is required"))
    }
}

/// The command groups.
#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Mint and revoke the one-time tokens that admit a launched machine's agent.
    #[command(subcommand, name = "join-token")]
    JoinToken(JoinTokenAction),
}

/// What to do with join tokens.
#[derive(Debug, Subcommand)]
pub(crate) enum JoinTokenAction {
    /// Mint a one-time join token for an instance.
    ///
    /// The token admits the agent that names this provider and instance in its hello,
    /// once, before it expires (the agent's `--instance-file` holds `PROVIDER:INSTANCE_ID`
    /// and its `--token-file` the token). Without --out the token alone is printed on
    /// stdout; with --out it is written to that file, created new with mode 0600 (a path
    /// that exists is refused before anything is asked of the server) and not printed.
    /// Minting again for the instance replaces its unused tokens.
    Mint(MintArgs),
    /// Drop every token of an instance, used or not (idempotent).
    Revoke(InstanceArgs),
}

/// The instance a join token is bound to.
#[derive(Debug, Args)]
pub(crate) struct InstanceArgs {
    /// The provider the instance belongs to; a pool that admits one provider accepts its
    /// own name only.
    #[arg(long, value_name = "NAME")]
    pub(crate) provider: String,
    /// The instance's id, as the agent's `--instance-file` names it.
    #[arg(long, value_name = "ID")]
    pub(crate) instance_id: String,
}

impl InstanceArgs {
    /// The body of `DELETE /v1/agent/join-tokens`.
    pub(crate) fn body(&self) -> Value {
        json!({ "provider": self.provider, "instance_id": self.instance_id })
    }
}

/// `join-token mint`: the instance, how long the token lasts and where it goes.
#[derive(Debug, Args)]
pub(crate) struct MintArgs {
    #[command(flatten)]
    pub(crate) instance: InstanceArgs,
    /// How long the token may be used (the server's default, 30 minutes, when absent; at
    /// most 24h).
    #[arg(long, value_parser = parse_duration)]
    pub(crate) ttl: Option<Duration>,
    /// Write the token to this new file (mode 0600, the token and a newline) instead of
    /// printing it.
    #[arg(long, value_name = "FILE")]
    pub(crate) out: Option<PathBuf>,
}

impl MintArgs {
    /// The body of `POST /v1/agent/join-tokens`; the ttl is sent only when given.
    pub(crate) fn body(&self) -> Value {
        let mut body = self.instance.body();
        if let Some(ttl) = self.ttl {
            body["ttl_seconds"] = json!(ttl.as_secs());
        }
        body
    }
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
