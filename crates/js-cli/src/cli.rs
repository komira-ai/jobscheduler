//! The command line: flags only, never environment variables.

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, CommandFactory, Parser, Subcommand};
use serde_json::{Value, json};

use crate::failure::Failure;

/// The default for `--wait-pending`.
const WAIT_PENDING: &str = "60s";

/// What `js lease run` says: the command only refuses.
pub(crate) const RUN_REMOVED: &str =
    "js lease run was removed: leases are no longer renewed; submit a job";

/// What `js lease acquire --ttl` says: the flag only refuses.
pub(crate) const TTL_REMOVED: &str = concat!(
    "--ttl was removed: a lease has no time to live ",
    "(it ends when released or at its maximum wall time); remove the flag"
);

/// The command line of `js`. The four global flags may stand before or after the
/// subcommand.
#[derive(Debug, Parser)]
#[command(
    name = "js",
    version,
    about = "Acquires and releases leases on a lease server, and makes the calls of the job service"
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

/// The whole command line: the lease commands of [`Cli`] and the job commands generated
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
    /// Work with leases.
    #[command(subcommand)]
    Lease(Action),
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

/// What to do with leases.
#[derive(Debug, Subcommand)]
pub(crate) enum Action {
    /// Ask for a lease and print it.
    Acquire(AcquireArgs),
    /// Give a lease back.
    Release(IdArgs),
    /// List the leases the server holds.
    List,
    /// Removed: leases are no longer renewed. Refuses, and sends nothing.
    #[command(hide = true)]
    Run {
        /// Whatever was given: swallowed, since the command only refuses.
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "ARGS"
        )]
        _words: Vec<OsString>,
    },
    /// Queue a native job (a program run on the host, no image) on a held lease.
    ///
    /// The lease's host must run native jobs. The program is an absolute path, or
    /// `@bundle/<entrypoint>` with --bundle. Prints the run's id; the job's state and
    /// output are the server's, at `/v1/leases/<id>/runs`.
    Native(NativeArgs),
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
    /// Removed: a lease has no time to live. Any value is read and refused.
    #[arg(long, hide = true, allow_hyphen_values = true, value_name = "DUR")]
    pub(crate) ttl: Option<String>,
    /// A label of the lease, `key=value`; repeat for more.
    #[arg(long = "label", value_name = "KEY=VALUE", value_parser = parse_label)]
    pub(crate) labels: Vec<(String, String)>,
    /// The validation run the lease serves.
    #[arg(long, value_name = "ID")]
    pub(crate) validation_run: Option<String>,
    /// A probe: place the lease on this host alone, past its cordon (the server refuses it
    /// by name when the host is unknown, not cordoned, not serving, too small or not one
    /// the server can probe).
    #[arg(long, value_name = "HOST")]
    pub(crate) probe_host: Option<String>,
    /// The request's id, of your choosing (1 to 63 of `a-z`, `A-Z`, `0-9`, `.`, `_`,
    /// `-`): an acquire repeated with the same id and flags answers the lease the first one
    /// made instead of taking a second, and a whole-node request waits for a host instead
    /// of being refused for lack of one. Every flag that shapes the request counts;
    /// `--wait-pending` does not. The same id with another request is refused
    /// `request-id-conflict` while the first lease is on the server.
    #[arg(long, value_name = "ID")]
    pub(crate) request_id: Option<String>,
    /// How long to wait for a start the provider has not confirmed (202) to settle.
    #[arg(long, default_value = WAIT_PENDING, value_parser = parse_duration)]
    pub(crate) wait_pending: Duration,
}

impl AcquireArgs {
    /// The body of `POST /v1/leases`. A probe host and a request id are each sent only
    /// when asked for, so a server that predates the field takes the body of every other
    /// start.
    pub(crate) fn body(&self) -> Value {
        let labels: serde_json::Map<String, Value> = self
            .labels
            .iter()
            .map(|(key, value)| (key.clone(), json!(value)))
            .collect();
        let mut body = json!({
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
        });
        if let Some(host) = &self.probe_host {
            body["probe_host"] = json!(host);
        }
        if let Some(id) = &self.request_id {
            body["request_id"] = json!(id);
        }
        body
    }
}

/// A lease id.
#[derive(Debug, Args)]
pub(crate) struct IdArgs {
    /// The lease, as `acquire` printed it.
    #[arg(value_parser = parse_id)]
    pub(crate) id: String,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `lease acquire`'s flags alone, as clap reads them.
    #[derive(Debug, Parser)]
    struct Acquire {
        #[command(flatten)]
        args: AcquireArgs,
    }

    const BASE: [&str; 5] = ["acquire", "--class", "c", "--max-wall", "1h"];

    fn acquire(extra: &[&str]) -> AcquireArgs {
        Acquire::try_parse_from(BASE.iter().chain(extra))
            .unwrap()
            .args
    }

    /// Asserts the body of a start: without `--probe-host` it has no `probe_host` key at
    /// all (a server that predates the field refuses an unknown key, even a null one, so
    /// every other start keeps working there); with it the key names the host and the rest
    /// of the body is the same; and the body never has a `ttl_seconds` key, whatever
    /// `--ttl` was given. It catches a key sent when unset, a flag parsed and dropped, a
    /// probe that changes the rest of the start, and a ttl that still reaches the server.
    #[test]
    fn the_probe_host_is_in_the_body_only_when_asked_for() {
        let plain = acquire(&[]).body();
        let expected = json!({
            "class": "c",
            "resources": { "cpu_millis": 0, "memory_bytes": 0, "gpus": 0, "gpu_class": null },
            "max_wall_seconds": 3600,
            "labels": {},
            "validation_run": null,
        });
        assert_eq!(plain, expected);
        let probe = acquire(&["--probe-host", "mac-07"]).body();
        let mut with = expected;
        with["probe_host"] = json!("mac-07");
        assert_eq!(probe, with);
        assert!(
            acquire(&["--ttl", "60s"])
                .body()
                .get("ttl_seconds")
                .is_none()
        );
    }

    /// Asserts the body of a start with `--request-id`: without the flag the body has no
    /// `request_id` key at all (a server that predates the field refuses an unknown key);
    /// with it the key carries the id and the rest of the body is the same. It catches a
    /// key sent when unset (which an older server would refuse for every start) and a flag
    /// parsed and dropped.
    #[test]
    fn the_request_id_is_in_the_body_only_when_asked_for() {
        let plain = acquire(&[]).body();
        assert!(plain.get("request_id").is_none(), "{plain}");
        let named = acquire(&["--request-id", "nightly-42"]).body();
        let mut with = plain;
        with["request_id"] = json!("nightly-42");
        assert_eq!(named, with);
    }
}
