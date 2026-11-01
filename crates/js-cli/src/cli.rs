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
    /// authority that issued it); only those certificates are trusted then. Optional:
    /// without it an `https://` server is verified against the public roots bundled in the
    /// binary, never the platform's store, because the places it is found can be redirected
    /// by the environment.
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
    /// the deadline short of the server's expiry (a failed renewal is tried again after a
    /// sixth of the period, at least a second, until then), it stops the command the same way and
    /// exits 7; a command that cannot be started exits 127. The held-lease line is
    /// printed on stderr, so the command owns stdout.
    Run(RunArgs),
    /// Queue a native job (a program run on the host, no image) on a held lease.
    ///
    /// The lease's host must run native jobs. The program is an absolute path, or
    /// `@bundle/<entrypoint>` with --bundle. Prints the run's id; the job's state and
    /// output are the server's, at `/v1/leases/<id>/runs`.
    Native(NativeArgs),
    /// Work with the one-time join tokens of hosts that are launched.
    #[command(subcommand)]
    JoinToken(JoinTokenAction),
}

/// What to do with join tokens.
#[derive(Debug, Subcommand)]
pub(crate) enum JoinTokenAction {
    /// Mint a one-time join token for a host and print its sha256 and expiry.
    ///
    /// The server binds the token to PROVIDER:INSTANCE and retires it at the host's first
    /// hello. The token is written to --out (a new file, mode 0600) or, when standard
    /// output is not a terminal, to standard output, so that it can be piped to
    /// `js-agent enroll`; it is never printed to a terminal and never taken from or put on a
    /// command line. The sha256 of the token and its expiry (the server's clock in
    /// milliseconds) go to standard error; with --json they are a JSON object on standard
    /// error, and the token stays on standard output.
    Mint(MintArgs),
}

/// `lease join-token mint`: the host the token is for, how long it lasts, where it goes.
#[derive(Debug, Args)]
pub(crate) struct MintArgs {
    /// The provider of the host: the name of the pool the host joins.
    #[arg(long, value_name = "ID")]
    pub(crate) provider: String,
    /// The instance the token is bound to: the id the host's instance file names.
    #[arg(long, value_name = "ID")]
    pub(crate) instance: String,
    /// How long the token can be used to join (the server's default when absent). The server
    /// refuses more than a day.
    #[arg(long, value_parser = parse_duration)]
    pub(crate) ttl: Option<Duration>,
    /// Write the token to this new file (mode 0600) instead of standard output. The file
    /// must not exist; the directory must.
    #[arg(long, value_name = "PATH")]
    pub(crate) out: Option<PathBuf>,
}

impl MintArgs {
    /// The body of `POST /v1/agent/join-tokens`: the two ids, and the ttl when given.
    pub(crate) fn body(&self) -> Value {
        let mut body = json!({
            "provider": self.provider,
            "instance_id": self.instance,
        });
        if let Some(ttl) = self.ttl {
            body["ttl_seconds"] = json!(ttl.as_secs());
        }
        body
    }
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

/// `lease native`: the lease, the program and its limits.
#[derive(Debug, Args)]
pub(crate) struct NativeArgs {
    /// The lease, as `acquire` printed it.
    #[arg(value_parser = parse_id)]
    pub(crate) id: String,
    /// The program to run: an absolute path, or `@bundle/<entrypoint>` of the bundle.
    #[arg(long)]
    pub(crate) program: String,
    /// One argument of the program; repeat for more, in order.
    #[arg(long = "arg", value_name = "ARG", allow_hyphen_values = true)]
    pub(crate) args: Vec<String>,
    /// The most network the job may use.
    #[arg(long, value_enum, default_value_t = Network::Off)]
    pub(crate) network: Network,
    /// The bundle the program comes from, pinned by digest:
    /// `name[:tag]@sha256:<manifest digest>`.
    #[arg(long, value_name = "REF")]
    pub(crate) bundle: Option<String>,
    /// CPU, in thousandths of a core.
    #[arg(long)]
    pub(crate) cpu_millis: u64,
    /// Memory, in bytes.
    #[arg(long)]
    pub(crate) memory_bytes: u64,
    /// The longest the job may run (what the lease has left when absent).
    #[arg(long, value_parser = parse_duration)]
    pub(crate) wall: Option<Duration>,
}

/// How much network a native job may use.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub(crate) enum Network {
    /// None.
    Off,
    /// The loopback interface only.
    Loopback,
    /// The network.
    On,
}

impl Network {
    /// The level as the server's body names it.
    fn name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Loopback => "loopback",
            Self::On => "on",
        }
    }
}

impl NativeArgs {
    /// The body of `POST /v1/leases/{id}/runs`: no image, the native object, the limits,
    /// and the bundle and the wall time only when given.
    pub(crate) fn body(&self) -> Value {
        let mut limits = json!({
            "cpu_millis": self.cpu_millis,
            "memory_bytes": self.memory_bytes,
        });
        if let Some(wall) = self.wall {
            limits["wall_ms"] = json!(wall.as_millis());
        }
        let mut body = json!({
            "native": {
                "program": self.program,
                "args": self.args,
                "network": self.network.name(),
            },
            "limits": limits,
        });
        if let Some(bundle) = &self.bundle {
            body["bundle"] = json!(bundle);
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
