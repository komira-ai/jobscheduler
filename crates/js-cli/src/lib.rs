//! `js`: the command-line client of `js-lease-server`. It acquires and releases leases on
//! whole GPU hosts; and it makes the calls
//! of the job service (`jobscheduler.v1.JobService`), one command per call, with three
//! commands written by hand that loop over several calls: `job wait`, `job follow` and
//! `job run` (see "Job commands" below).
//!
//! The library is the program: [`run_with`] takes the arguments, the streams to print
//! to and the [`Services`] (the waiter and the signal source), and `main`
//! calls [`main`], which supplies the program's own.
//!
//! # Flags
//!
//! Configuration is flags only: `js` reads no environment variable. The four global
//! flags may stand before or after the subcommand.
//!
//! | flag | meaning |
//! |---|---|
//! | `--server <url>` | required: `https://host:port`; `http://` only for `localhost`, `127.0.0.0/8` and `::1` (a bearer token never crosses a network in clear); no user, path or query |
//! | `--token-file <path>` | required: a regular file owned by you, mode 0600 or 0400, holding one token of printable ASCII (the server checks its own token file the same way). On the job service it is the token of one of the server's clients (its `--token-file`, the client `default`, or one of its `--client-token` entries), and the jobs it submits carry that client's name |
//! | `--ca-cert <path>` | optional for `https://`: a PEM file with the certificate to trust (the server's, or the authority that issued it; a system bundle such as `/etc/ssl/certs/ca-certificates.crt` works), and then only those are trusted. Without it the public roots bundled in the program (`webpki-roots`) are trusted, never the platform's store. Ignored for `http://` |
//! | `--json` | machine output: one JSON object per line (for `list` the server's own body), errors on stderr as `{"error": kind, "message": text}` |
//!
//! No proxy setting is read, no redirect is followed and the token is sent to the one
//! host named, over TLS unless that host is the machine itself.
//!
//! # Commands
//!
//! ```text
//! js lease acquire --class gpu-host [--gpus N --gpu-class X --cpu-millis N --memory-bytes N]
//!                  --max-wall <dur> [--label k=v ...] [--validation-run ID]
//!                  [--probe-host HOST] [--request-id ID] [--wait-pending <dur>]
//! js lease release <id>
//! js lease list
//! js lease native <id> --program <path|@bundle/entry> [--arg A ...] [--network off|loopback|on]
//!                 [--bundle name[:tag]@sha256:<digest>] --cpu-millis N --memory-bytes N
//!                 [--wall <dur>]
//! js join-token mint --provider NAME --instance-id ID [--ttl <dur>] [--out FILE]
//! js join-token revoke --provider NAME --instance-id ID
//! ```
//!
//! A lease has no time to live: nothing a client does keeps it alive, and it ends when it
//! is released or at its maximum wall time (`--max-wall`). `js lease acquire --ttl` and
//! `js lease run` are refused (exit 2) with a message saying they were removed; nothing is
//! sent to the server. `--ttl` of `join-token mint` is another thing, below.
//!
//! `join-token mint` asks for a one-time join token (`POST /v1/agent/join-tokens`, behind
//! the client token like the lease commands) for the machine a provider launched as
//! `NAME:ID`; the agent that runs there names the same pair in its `--instance-file` and
//! reads the token from its `--token-file`. `--ttl` is how long the token may be used
//! (default 30 minutes, at most 24h). Without `--out` the token alone is printed on stdout
//! (`--json`: `{"token", "expires_at_ms"}`); it is the only command that prints a secret.
//! With `--out FILE` the file is created new with mode 0600 and holds the token and a
//! newline, stdout says where it went and when it expires (`expires` is the server's clock
//! in milliseconds) and never shows the token, and a `FILE` that exists is refused (exit 2)
//! before the server is asked for anything; a mint that fails leaves no file. Minting again
//! for an instance replaces its unused tokens. `join-token revoke` drops every token of
//! the instance, used or not (idempotent). A server that admits no agents has no such route
//! and answers `join-tokens-unsupported` (exit 3); a provider the pool does not admit, an
//! id that is not valid or a `--ttl` above a day is exit 4.
//!
//! `native` queues a native job (a program run on the host, no image) on a held lease and
//! prints `run <id> <state>`. The server refuses it (exit 3) when the lease's host does not
//! run native jobs (`native-unsupported`) or holds no bundle store for `--bundle`
//! (`bundle-unsupported`); a body that breaks a rule (a program that is not an absolute
//! path, a bundle without a digest) is exit 4.
//!
//! `--probe-host HOST` asks for a probe: the lease is placed on that host alone, past the
//! host's cordon, by a server that places with `js-sched` (`--placement js-sched`). A
//! server that places no probe answers `probe-unsupported`, and one the host's state rules
//! out `probe-host-unknown`, `-not-serving`, `-not-cordoned`, `-draining`, `-mismatch` or
//! `-too-small` (all exit 3); a host with no room yet is `no-capacity`, as for any start.
//! The flag is sent only when given, so a server that predates it takes every other start.
//!
//! `--request-id ID` names the start (1 to 63 of `a-z`, `A-Z`, `0-9`, `.`, `_`, `-`, of
//! your choosing). An `acquire` repeated with the same id and the same flags is answered
//! with the lease the first one made (held, or still pending) and takes no second host,
//! for as long as that lease is on the server; the same id with another request (any flag
//! but `--wait-pending` changed) is refused `request-id-conflict` (exit 3). A `whole-node` start with an id waits on the server for
//! a host instead of being refused `no-capacity`: it is pending until a host is set aside,
//! drained and prepared for it, so give `--wait-pending` as long as you mean to wait (the
//! pending lease is released when it runs out, which cancels the request). The flag is
//! sent only when given.
//!
//! Durations are whole seconds: `90`, `90s`, `15m`, `2h`. `acquire` prints
//! `lease <id> host <host> ends <ms>` (`ends` is the server's clock, in milliseconds, when
//! the lease reaches its maximum wall time; `--json` has it as `ends_at_ms`, and `list`
//! prints `<id> <state> host <host> ends <ms>`). A start the provider has not confirmed (202) is looked at in the
//! list every two seconds until it is held or gone, for at most `--wait-pending`
//! (default 60s); the pause is asked of the [`Waiter`], the only thing that lets time
//! pass. A lease still pending then is released, so it is not held later with nobody to
//! release it.
//!
//! A start is sent once. When its answer is lost (the connection fails, the server
//! answers 5xx or something this client does not know, or the body is unusable) the
//! lease may exist, and `js` exits 1 saying so and telling you to run `js lease list`
//! before trying again (the JSON kind is `acquire-unconfirmed`); an `acquire` that gave
//! `--request-id` can instead be run again as it was. `acquire` does no more.
//!
//! # Job commands
//!
//! ```text
//! js job submit [--id ID] --spec.image REF [--spec.args A ...] [--spec.env K=V ...]
//!               [--spec.needs.cpu-millis N ...] [--spec.wall-limit-seconds <dur>] ...
//! js job get --job-id ID          js job cancel --job-id ID
//! js job list [--states S ...] [--labels K=V ...] [--page-size N] [--page-token T]
//! js job result --job-id ID       js job logs --job-id ID
//! js host list                    js host cordon --host-id ID [--cordon-secs <dur>]
//! js host uncordon --host-id ID   js pool list     js provider list
//! js usage [--from-ms N] [--to-ms N] [--host-id ID] [--job-id ID] ...
//! js job wait --job-id ID         js job follow --job-id ID [--stream stdout|stderr]
//! js job run <the flags of job submit>
//! ```
//!
//! These commands are not written here: `cargo xtask gen-job-surface` generates their table
//! from the service definition in `js-proto` ([`jobs::surface`]), and [`jobs`] builds the
//! command line from it, so a call added to the definition is a command with no hand-written
//! code. `js <command> --help` shows each flag with the comment of its field. The global
//! flags mean what they mean for the lease commands: the same `--server` rule, token file
//! and roots, over gRPC (HTTP/2) to the one origin.
//!
//! How a call is named: a call is a verb and a noun. When its request has exactly one
//! singular, non-`optional` scalar field (its subject, whose flag is required) named
//! `<owner>_id`, the command is `<owner> <verb>` when the noun is the owner (`GetJob` is
//! `job get`) and `<owner> <noun>` otherwise (`GetResult` is `job result`). Otherwise it is
//! `<noun> <verb>` with the noun's trailing `s` dropped (`ListHosts` is `host list`), and a
//! `Get` is the noun alone (`GetUsage` is `usage`).
//!
//! How a field is a flag: the field names in kebab case, joined by dots through nested
//! messages, so there is one flag per leaf (`--spec.needs.cpu-millis 1500`). A scalar takes
//! one value (`true`/`false` for a bool); a repeated field repeats the flag; a map is
//! `--labels KEY=VALUE`, repeated; an integer field named `*_seconds` or `*_secs` takes a
//! duration (`90`, `90s`, `15m`, `2h`); an enum takes its value's short name, its name
//! without the enum's shared prefix in lower case with dashes (`--states failed-to-start`).
//! A field the flags cannot spell (`bytes`, a repeated message such as `spec.ports`, a map
//! of messages, a `oneof` member) is `--<name>-file PATH`, a file holding the field's value
//! in proto3 JSON (`--spec.ports-file ports.json` holding
//! `[{"hostPort": 8080, "containerPort": 80}]`).
//!
//! Output: one `path: value` line per field of the answer that is set (`job.state:
//! queued`, `jobs[0].id: ...`), or with `--json` the answer in proto3 JSON on one line. A
//! log tail (`job logs`, `job result`) prints its label on a line and then its text: the
//! standard output tail, then the standard error tail, and `truncated: true` when a stream
//! was cut.
//!
//! `job submit` is idempotent on the job's id (design section 5.3): the same id with the
//! same job answers from the job the first submit made. Without `--id`, `js` draws 128
//! random bits and uses them in hexadecimal, and prints `js: job id <id>` on stderr before
//! it sends. When the request was sent and no answer was read (the connection broke, the
//! call timed out, the server answered `UNAVAILABLE`, `UNKNOWN` or `INTERNAL`), the job may
//! exist: `js` says `js: the job may exist; run the same command again with --id <id>` and
//! exits 9 (JSON kind `unconfirmed`). A request that was never sent (the connection or TLS
//! failed first, or the token was refused) is exit 1 (`not-sent`) and changed nothing. A
//! definite refusal is exit 4 for `INVALID_ARGUMENT` (the request itself, such as a wall
//! limit of 0) and exit 3 for any other code (`no-capable-host`, an unknown job).
//!
//! ## The hand-written job commands
//!
//! Three commands of the `job` group are written by hand, not generated (`src/jobs/wait.rs`,
//! `follow.rs`, `run.rs`), each over a loop of `js_client::jobs` that pauses only through the
//! injected job waiter ([`Services::with_job_waiter`]; tokio's timer in the program):
//!
//! - `job wait --job-id ID` reads the job (`GetJob`) every two seconds until it has ended,
//!   and a failed job's result (`GetResult`) to tell a wall limit from another failure;
//!   prints `job <id> <end>` (JSON `{"id", "outcome"}`) and exits with the status of that end
//!   (the table below: 0, or 10 to 14). A call that fails is not retried, and is exit 15
//!   (`undetermined`), as is a state this `js` does not know.
//! - `job follow --job-id ID [--stream stdout|stderr]` writes the stream (stdout unless
//!   named) to stdout from its first byte to its end, exactly as the job wrote it: each
//!   `GetLogs` asks the server to wait for new bytes and starts at the offset the last one
//!   answered, until the server says the job has ended and every byte was read (`eof`).
//!   Bytes the log store does not hold are said on stderr (`js: stdout bytes 0 to 4 are not
//!   in the log store`). A pause of one second follows an answer with no data; a follow the
//!   server refuses as busy is tried again after five seconds, and one whose answer is lost
//!   (or whose connection failed) after two, three times in a row at most; any other failure
//!   exits as a job command's does. `job logs --follow true` is one call, one page.
//! - `job run` takes the flags of `job submit`, and its id as `submit` draws or takes it. The
//!   submit is sent again with that id, up to three times in all with two seconds between,
//!   when its answer is lost or its connection failed, so it makes one job at most; a submit
//!   that fails still exits as `job submit` does (1, 3, 4 or 9). The job's stdout and stderr
//!   are then written to `js`'s own as `follow` writes them while the job is read as `wait`
//!   reads it, each on a connection of its own; once both streams are at their end, the end
//!   is printed on stderr and `js` exits with `wait`'s status for it. A stream that cannot be
//!   followed is said on stderr and does not change the status. `run` installs its SIGINT,
//!   SIGTERM and SIGHUP handlers before it submits: the first signal before the job has
//!   ended is said on stderr and cancels the job (`CancelJob`, once, at the wait's next
//!   pause), and `js` goes on waiting for the end the job reaches; a second ends `js` at once with 128 plus its number,
//!   saying the job may still be running.
//!
//! Text the server sends in an error or a reason is printed without its control
//! characters.
//!
//! # Exit status
//!
//! | status | meaning |
//! |---|---|
//! | 0 | held, freed, listed; for `job wait` and `job run`, the job succeeded |
//! | 1 | the server or the network failed, or the token was refused (401); a start whose answer was lost (the lease may exist: run `js lease list`); a job command whose request was not sent (`not-sent`) |
//! | 2 | a flag or a file it names is wrong, including what clap refuses |
//! | 3 | refused: a job command's definite refusal other than `INVALID_ARGUMENT` (`no-capable-host`, an unknown job); `no-capacity`, `no-fit`, `unsupported-class`, `lease-ended`, `no-capable-host`, `exclusive-not-ready`, `request-id-conflict`, `unknown-lease`, `not-held`, or a pending start that was dropped (`pending-dropped`) |
//! | 4 | the request itself was refused: a job command answered `INVALID_ARGUMENT`; a start-contract rule (`unknown-class`, `reserved-label`, `invalid-label`, `below-class-minimum`, `wall-time-not-positive`) or a body the server cannot read |
//! | 5 | a pending start was not confirmed within `--wait-pending` |
//! | 6 | `release`: the stop was accepted but is not confirmed; the server retries it |
//! | 9 | a job command sent its request and read no answer, so the call may have taken effect (`unconfirmed`); for `job submit` and `job run` the job may exist and the same command with the same `--id` is safe to run again |
//! | 10 | `job wait`, `job run`: the job failed (a nonzero exit code, or stopped for its memory) |
//! | 11 | `job wait`, `job run`: the job was cancelled |
//! | 12 | `job wait`, `job run`: the job timed out (stopped at its wall limit) |
//! | 13 | `job wait`, `job run`: the job was lost |
//! | 14 | `job wait`, `job run`: the job failed to start |
//! | 15 | `job wait`, `job run`: how the job ended could not be told: a call failed, or the job is in a state this `js` does not know (`undetermined`) |
//! | 129, 130, 143 | `job run`: a second SIGHUP, SIGINT or SIGTERM ended `js` before the job ended (`interrupted`) |
//!
//! The reason is on stderr (`js: refused: no-capacity`), and never holds the token.

mod api;
mod cli;
mod events;
mod failure;
pub mod jobs;
mod join_token;
mod leases;
mod render;
mod signals;

use std::ffi::OsString;
use std::io::Write;
use std::process::ExitCode;

use clap::{ArgMatches, FromArgMatches};

use cli::{Action, Cli, Command};
use failure::Failure;
use js_client::{Client, REQUEST_LIMIT};
use render::Render;

pub use events::{ForwardedSignal, Services, SignalSource, Signaller, ThreadWaiter, Waiter};
pub use failure::exit;

/// The whole command line of `js`, as it parses: the lease commands, the job commands
/// generated from the job service, and the global flags. Read-only: a caller that walks
/// its subcommands and flags (a check that the documentation names only commands that
/// exist) sees the tree [`run_with`] parses with.
#[must_use]
pub fn command() -> clap::Command {
    cli::command()
}

/// The program: runs with the process's arguments and standard streams, and the
/// program's own [`Services`].
pub fn main() -> ExitCode {
    let status = run_with(
        std::env::args_os().collect(),
        &mut std::io::stdout(),
        &mut std::io::stderr(),
        Services::system(),
    );
    ExitCode::from(status)
}

/// Runs the command line `args` (the program name first). Results go to `out`, usage
/// text for `--help` and `--version` too; failures go to `err`. Returns the exit status documented in the crate docs.
pub fn run_with(
    args: Vec<OsString>,
    out: &mut dyn Write,
    err: &mut dyn Write,
    services: Services,
) -> u8 {
    match cli::command().try_get_matches_from(args) {
        Ok(matches) => execute(&matches, out, err, services),
        Err(usage) => usage_exit(&usage, out, err),
    }
}

/// Prints a usage error where clap would and picks the exit status clap picks.
fn usage_exit(usage: &clap::Error, out: &mut dyn Write, err: &mut dyn Write) -> u8 {
    let _ = match usage.use_stderr() {
        true => write!(err, "{usage}"),
        false => write!(out, "{usage}"),
    };
    u8::try_from(usage.exit_code()).unwrap_or(exit::USAGE)
}

/// Runs a parsed command line, a job command or a lease command, and prints its failure,
/// if any.
fn execute(
    matches: &ArgMatches,
    out: &mut dyn Write,
    err: &mut dyn Write,
    services: Services,
) -> u8 {
    let render = Render::new(matches.get_flag("json"));
    let done = match jobs::chosen(matches) {
        Some(chosen) => jobs::execute(chosen, matches, render, out, err, services),
        None => lease(matches, render, out, services),
    };
    match done {
        Ok(code) => code,
        Err(failure) => {
            let _ = err.write_all(render.failure(&failure).as_bytes());
            failure.parts().0
        }
    }
}

/// Runs a lease command.
fn lease(
    matches: &ArgMatches,
    render: Render,
    out: &mut dyn Write,
    services: Services,
) -> Result<u8, Failure> {
    // clap already checked `matches` against these commands, so this does not fail; its
    // error is made up front.
    let cli = Cli::from_arg_matches(matches)
        .ok()
        .ok_or(Failure::Usage("not a lease command".to_owned()))?;
    dispatch(cli, render, out, services)
}

fn dispatch(
    cli: Cli,
    render: Render,
    out: &mut dyn Write,
    services: Services,
) -> Result<u8, Failure> {
    let server = cli.need_server()?;
    let token_file = cli.need_token_file()?;
    let client = Client::connect(server, token_file, cli.ca_cert.as_deref(), REQUEST_LIMIT)?;
    match cli.command {
        Command::Lease(action) => lease_action(action, &client, render, out, &services),
        Command::JoinToken(action) => join_token::run(&client, render, action, out),
    }
}

/// Runs a lease command over a connected client.
fn lease_action(
    action: Action,
    client: &Client,
    render: Render,
    out: &mut dyn Write,
    services: &Services,
) -> Result<u8, Failure> {
    match action {
        Action::Acquire(args) => leases::acquire(client, render, &args, &*services.waiter, out),
        Action::Release(args) => leases::release(client, render, &args, out),
        Action::List => leases::list(client, render, out),
        Action::Run { .. } => Err(Failure::Usage(cli::RUN_REMOVED.to_owned())),
        Action::Native(args) => leases::native(client, render, &args, out),
    }
}
