//! The command line: flags only, never environment variables.

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use serde_json::{Value, json};

use crate::failure::Failure;
use crate::tag::{self, RunTag};

/// The default for `--wait-pending`.
const WAIT_PENDING: &str = "60s";

/// The default for `--stop-grace`.
const STOP_GRACE: &str = "30s";

/// The command line of `js`. The four global flags may stand before or after the
/// subcommand.
#[derive(Debug, Parser)]
#[command(
    name = "js",
    version,
    about = "Acquires, renews and releases leases on a lease server"
)]
pub struct Cli {
    /// The lease server, such as `https://leases.example:7300`. Plain `http://` is
    /// accepted for a loopback host only: the bearer token never crosses a network in
    /// clear. Required.
    #[arg(long, global = true, value_name = "URL")]
    pub(crate) server: Option<String>,
    /// A file holding the bearer token: a regular file owned by you, mode 0600 or 0400.
    /// Required.
    #[arg(long, global = true, value_name = "PATH")]
    pub(crate) token_file: Option<PathBuf>,
    /// A PEM file with the certificate to trust for `https://` (the server's, or the
    /// authority that issued it). Required for `https://`: the platform's roots are not
    /// read, because the places they are found can be redirected by the environment.
    #[arg(long, global = true, value_name = "PATH")]
    pub(crate) ca_cert: Option<PathBuf>,
    /// Print machine output (one JSON object per line, or the server's own JSON for
    /// `list`) instead of text; errors too.
    #[arg(long, global = true)]
    pub(crate) json: bool,
    #[command(subcommand)]
    pub(crate) command: Command,
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
    /// Work with leases.
    #[command(subcommand)]
    Lease(Action),
}

/// What to do with leases.
#[derive(Debug, Subcommand)]
pub(crate) enum Action {
    /// Ask for a lease and print it.
    Acquire(AcquireArgs),
    /// Extend a held lease.
    Renew(RenewArgs),
    /// Give a lease back.
    Release(IdArgs),
    /// List the leases the server holds.
    List,
    /// Acquire a lease, run a command while renewing it, then release it.
    ///
    /// The command is a child process with JS_LEASE_ID and JS_LEASE_HOST set in its
    /// environment: they are outputs of `js` to the command, not configuration of `js`,
    /// which reads no environment variable. js renews the lease at a third of the time
    /// the server's last answer left (at most --renew-every apart), forwards SIGINT,
    /// SIGTERM and SIGHUP to the command's process group, and when the command ends
    /// stops what it left running (SIGTERM, then SIGKILL after --stop-grace), releases
    /// the lease and exits with the command's status (128 plus the signal for a command
    /// a signal ended). If a renewal finds the lease gone, or none has succeeded before
    /// the deadline short of the server's expiry, it stops the command the same way and
    /// exits 7; a command that cannot be started exits 127. The held-lease line is
    /// printed on stderr, so the command owns stdout.
    Run(RunArgs),
}

/// What a start asks for.
#[derive(Debug, Args)]
pub(crate) struct AcquireArgs {
    /// The resource class, such as `gpu-host`.
    #[arg(long)]
    pub(crate) class: String,
    /// How many GPUs the request needs.
    #[arg(long, default_value_t = 0)]
    pub(crate) gpus: u64,
    /// The GPU class the request needs, such as `nvidia-a100`.
    #[arg(long)]
    pub(crate) gpu_class: Option<String>,
    /// CPU, in thousandths of a core.
    #[arg(long, default_value_t = 0)]
    pub(crate) cpu_millis: u64,
    /// Memory, in bytes.
    #[arg(long, default_value_t = 0)]
    pub(crate) memory_bytes: u64,
    /// The longest the lease may last in all, such as `2h`.
    #[arg(long, value_parser = parse_duration)]
    pub(crate) max_wall: Duration,
    /// How long the lease lasts without a renewal (the server's default when absent).
    #[arg(long, value_parser = parse_duration)]
    pub(crate) ttl: Option<Duration>,
    /// A label of the lease, `key=value`; repeat for more.
    #[arg(long = "label", value_name = "KEY=VALUE", value_parser = parse_label)]
    pub(crate) labels: Vec<(String, String)>,
    /// The validation run the lease serves.
    #[arg(long, value_name = "ID")]
    pub(crate) validation_run: Option<String>,
    /// How long to wait for a start the provider has not confirmed (202) to settle.
    #[arg(long, default_value = WAIT_PENDING, value_parser = parse_duration)]
    pub(crate) wait_pending: Duration,
}

impl AcquireArgs {
    /// The body of `POST /v1/leases`; `lease run` adds its tag to the labels.
    pub(crate) fn body(&self, tag: Option<&RunTag>) -> Value {
        let mut labels: serde_json::Map<String, Value> = self
            .labels
            .iter()
            .map(|(key, value)| (key.clone(), json!(value)))
            .collect();
        labels.extend(tag.map(|tag| (tag::KEY.to_owned(), json!(tag.value()))));
        json!({
            "class": self.class,
            "resources": {
                "cpu_millis": self.cpu_millis,
                "memory_bytes": self.memory_bytes,
                "gpus": self.gpus,
                "gpu_class": self.gpu_class,
            },
            "max_wall_seconds": self.max_wall.as_secs(),
            "labels": labels,
            "validation_run": self.validation_run,
            "ttl_seconds": self.ttl.map(|ttl| ttl.as_secs()),
        })
    }
}

/// A lease id and, for a renewal, the ttl to renew for.
#[derive(Debug, Args)]
pub(crate) struct RenewArgs {
    /// The lease, as `acquire` printed it.
    #[arg(value_parser = parse_id)]
    pub(crate) id: String,
    /// How long the lease lasts from now (the server's default when absent).
    #[arg(long, value_parser = parse_duration)]
    pub(crate) ttl: Option<Duration>,
}

/// A lease id.
#[derive(Debug, Args)]
pub(crate) struct IdArgs {
    /// The lease, as `acquire` printed it.
    #[arg(value_parser = parse_id)]
    pub(crate) id: String,
}

/// `lease run`: a start, a renewal period and the command.
#[derive(Debug, Args)]
pub(crate) struct RunArgs {
    #[command(flatten)]
    pub(crate) acquire: AcquireArgs,
    /// The longest to wait between renewals. Without it, and in any case, a renewal is
    /// tried when a third of the time the server's last answer left is gone.
    #[arg(long, value_parser = parse_duration)]
    pub(crate) renew_every: Option<Duration>,
    /// How long what the command started gets to end after SIGTERM (when the command
    /// ends, or its lease is lost), before it is killed (SIGKILL).
    #[arg(long, default_value = STOP_GRACE, value_parser = parse_duration)]
    pub(crate) stop_grace: Duration,
    /// The command and its arguments, after `--`.
    #[arg(last = true, required = true, value_name = "COMMAND")]
    pub(crate) command: Vec<OsString>,
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

/// A label `key=value` with a non-empty key.
fn parse_label(text: &str) -> Result<(String, String), String> {
    match text.split_once('=') {
        Some((key, value)) if !key.is_empty() => Ok((key.to_owned(), value.to_owned())),
        _ => Err(format!("{text:?} is not key=value")),
    }
}

/// A lease id in the form the server prints, two runs of digits around a dash; anything
/// else could name another route once it is part of a path.
fn parse_id(text: &str) -> Result<String, String> {
    let digits = |part: &str| !part.is_empty() & part.bytes().all(|b| b.is_ascii_digit());
    match text.split_once('-') {
        Some((term, sequence)) if digits(term) & digits(sequence) => Ok(text.to_owned()),
        _ => Err(format!("{text:?} is not a lease id such as 1-7")),
    }
}
