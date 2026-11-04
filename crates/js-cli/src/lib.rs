//! `js`: the command-line client of `js-lease-server`. It acquires, renews and releases
//! leases on whole GPU hosts, and runs a command while it holds one; and it makes the calls
//! of the job service (`jobscheduler.v1.JobService`), one command per call (see "Job
//! commands" below).
//!
//! The library is the program: [`run_with`] takes the arguments, the streams to print
//! to and the [`Services`] (the waiter, the clock and the signal source), and `main`
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
//! | `--token-file <path>` | required: a regular file owned by you, mode 0600 or 0400, holding one token of printable ASCII (the server checks its own token file the same way) |
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
//!                  --max-wall <dur> [--ttl <dur>] [--label k=v ...] [--validation-run ID]
//!                  [--probe-host HOST] [--request-id ID] [--wait-pending <dur>]
//! js lease renew <id> [--ttl <dur>]
//! js lease release <id>
//! js lease list
//! js lease run  <the flags of acquire> [--renew-every <dur>] [--stop-grace <dur>]
//!               -- <command> [args...]
//! js lease native <id> --program <path|@bundle/entry> [--arg A ...] [--network off|loopback|on]
//!                 [--bundle name[:tag]@sha256:<digest>] --cpu-millis N --memory-bytes N
//!                 [--wall <dur>]
//! ```
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
//! but `--ttl` and `--wait-pending` changed) is refused `request-id-conflict` (exit 3). A `whole-node` start with an id waits on the server for
//! a host instead of being refused `no-capacity`: it is pending until a host is set aside,
//! drained and prepared for it, so give `--wait-pending` as long as you mean to wait (the
//! pending lease is released when it runs out, which cancels the request). On `run` the id
//! does not make a repeat safe, since every `run` adds its own random label (below), which
//! the server compares: a second `run` under the id while the first run's lease is on the
//! server is refused `request-id-conflict`. The flag is sent only when given.
//!
//! Durations are whole seconds: `90`, `90s`, `15m`, `2h`. `acquire` prints
//! `lease <id> host <host> expires <ms>` (`expires` is the server's clock in
//! milliseconds). A start the provider has not confirmed (202) is looked at in the
//! list every two seconds until it is held or gone, for at most `--wait-pending`
//! (default 60s); the pause is asked of the [`Waiter`], the only thing that lets time
//! pass. A lease still pending then is released, so it is not held later with nobody to
//! renew it.
//!
//! A start is sent once. When its answer is lost (the connection fails, the server
//! answers 5xx or something this client does not know, or the body is unusable) the
//! lease may exist, and `js` exits 1 saying so and telling you to run `js lease list`
//! before trying again (the JSON kind is `acquire-unconfirmed`); an `acquire` that gave
//! `--request-id` can instead be run again as it was. `acquire` does no more.
//!
//! `run` acquires like `acquire`, with one more label, `js-run=<128 random bits in hex>`,
//! and starts the command as a child process with `JS_LEASE_ID` and `JS_LEASE_HOST` set
//! in its environment. Those are outputs of `js` to the command; `js` itself reads no
//! environment variable. When the answer to its start is lost, `run` lists the leases,
//! releases the one that has its label, says so on stderr and exits 1 as `acquire` does
//! (once: it does not retry the list, so a start the server only finishes afterwards
//! stays held until its ttl).
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
//! # Renewal and the deadline
//!
//! `run` knows how long the lease lasts from the server's answers, not from what was
//! asked for: every answer to a start or a renewal carries the expiry (`expires_at_ms`),
//! and the time left is that less the [`Clock`] (the system's wall clock, which is
//! assumed to agree with the server's to within a few seconds). A renewal is tried when
//! a third of the time left has passed (`--renew-every`, when given, is the longest
//! wait), so a ttl the server clamps or defaults is followed. The renewal runs on a
//! thread of its own: a slow one blocks neither a signal nor the command's exit. A
//! renewal that fails for a reason other than the server saying the lease is gone (404 or
//! 409) is reported on stderr and tried again after a sixth of the period (at least one
//! second), as often as a try ends before the deadline, until one succeeds; a refusal is
//! the loss of the lease at once. The lease is given up at the deadline, one
//! such period before the server would expire it, and the command is stopped (below)
//! while the lease is still the server's to release. A request is limited to 30 seconds,
//! and a renewal to the time left before the deadline when that is shorter.
//!
//! # Signals and what the command leaves behind
//!
//! `run` installs its SIGINT, SIGTERM and SIGHUP handlers before it asks for the lease.
//! A signal while a start is pending releases the pending lease and ends `js` with 128
//! plus the signal's number, without starting the command; once the lease is held, a
//! signal is forwarded to the command, which ends the run the usual way. The command
//! runs in a process group of its own, so a forwarded signal reaches what it started as
//! well, and a command that reads a terminal is not in the terminal's foreground group
//! (use `acquire` and `release` around an interactive program).
//!
//! However the run ends (the command exits, a signal ends it, the lease is lost) `js`
//! then ends the whole process group before it releases the lease, so that nothing the
//! command started keeps running on a host the server may lease again: if any process of
//! the group is left, it is sent SIGTERM, and SIGKILL when one is still there
//! `--stop-grace` later (default 30s, looked at in steps of 100ms, every step asked of
//! the waiter like every other pause; a signal sent to `js` meanwhile ends the wait at
//! once). A lost lease sends the group SIGTERM at once, and SIGKILL after the grace
//! period when the command is still running; `js` then exits 7, and signals sent to it
//! meanwhile are still forwarded. On Linux the command is also killed (SIGKILL) if `js`
//! itself is killed, as it could not forward a SIGKILL (`PR_SET_PDEATHSIG`: it takes the
//! command only, not what the command started, and holds for as long as the thread that
//! started the command lives). If a status poll of a pending start fails, that lease is
//! released too. A panic of `js` kills the command's group and releases the lease. A
//! command that cannot be started gives exit 127, and the lease is released.
//!
//! The release is tried up to three times, with 3 seconds (asked of the waiter) between
//! tries, when the server cannot be reached or fails; a refusal ("not held", the lease
//! has expired) is the answer and is not tried again, and a 202 (the server retries the
//! stop) is reported and kept. If no try is answered, `run` says so, and what the
//! command's status was, and exits 8 in its place: the lease may stay held until its ttl.
//!
//! Text the server sends in an error or a reason is printed without its control
//! characters.
//!
//! # Exit status
//!
//! | status | meaning |
//! |---|---|
//! | 0 | held, renewed, freed, listed; for `run`, the command's own status |
//! | 1 | the server or the network failed, or the token was refused (401); a start whose answer was lost (the lease may exist: run `js lease list`); a job command whose request was not sent (`not-sent`) |
//! | 2 | a flag or a file it names is wrong, including what clap refuses |
//! | 3 | refused: a job command's definite refusal other than `INVALID_ARGUMENT` (`no-capable-host`, an unknown job); `no-capacity`, `no-fit`, `unsupported-class`, `lease-ended`, `no-capable-host`, `exclusive-not-ready`, `request-id-conflict`, `unknown-lease`, `not-held`, `expired`, or a pending start that was dropped (`pending-dropped`) |
//! | 4 | the request itself was refused: a job command answered `INVALID_ARGUMENT`; a start-contract rule (`unknown-class`, `reserved-label`, `invalid-label`, `below-class-minimum`, `wall-time-not-positive`) or a body the server cannot read |
//! | 5 | a pending start was not confirmed within `--wait-pending` |
//! | 6 | `release`: the stop was accepted but is not confirmed; the server retries it |
//! | 7 | `run`: the lease was lost (gone from the server, or not renewed before the deadline short of its expiry); the command was stopped |
//! | 8 | `run`: the release of the lease could not be confirmed after three tries, so it may stay held until its ttl; replaces the command's own status, which is said on stderr |
//! | 9 | a job command sent its request and read no answer, so the call may have taken effect (`unconfirmed`); for `job submit` the job may exist and the same command with the same `--id` is safe to run again |
//! | 127 | `run`: the command could not be started |
//! | 129, 130, 143 | `run`: SIGHUP, SIGINT or SIGTERM ended `js` while the start was pending (after the command started, the command's own status) |
//!
//! The reason is on stderr (`js: refused: no-capacity`), and never holds the token.

mod api;
mod cli;
mod command;
mod events;
mod failure;
pub mod jobs;
mod leases;
mod render;
mod renewals;
mod run;
mod signals;
mod tag;

use std::ffi::OsString;
use std::io::Write;
use std::process::ExitCode;

use clap::{ArgMatches, FromArgMatches};

use cli::{Action, Cli, Command};
use failure::Failure;
use js_client::{Client, REQUEST_LIMIT};
use render::Render;

pub use events::{
    Clock, ForwardedSignal, Services, SignalSource, Signaller, SystemClock, ThreadWaiter, Waiter,
};
pub use failure::exit;

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
/// text for `--help` and `--version` too; failures, and for `lease run` the held-lease
/// line, go to `err`. Returns the exit status documented in the crate docs.
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
        Some((command, own)) => {
            jobs::run(command, matches, own, render, out, err, &services.dialer)
        }
        None => lease(matches, render, out, err, services),
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
    err: &mut dyn Write,
    services: Services,
) -> Result<u8, Failure> {
    let cli = Cli::from_arg_matches(matches).map_err(|usage| Failure::Usage(usage.to_string()))?;
    dispatch(cli, render, out, err, services)
}

fn dispatch(
    cli: Cli,
    render: Render,
    out: &mut dyn Write,
    err: &mut dyn Write,
    services: Services,
) -> Result<u8, Failure> {
    let server = cli.need_server()?;
    let token_file = cli.need_token_file()?;
    let client = Client::connect(server, token_file, cli.ca_cert.as_deref(), REQUEST_LIMIT)?;
    let Command::Lease(action) = cli.command;
    match action {
        Action::Acquire(args) => leases::acquire(&client, render, &args, &*services.waiter, out),
        Action::Renew(args) => leases::renew(&client, render, &args, out),
        Action::Release(args) => leases::release(&client, render, &args, out),
        Action::List => leases::list(&client, render, out),
        Action::Run(args) => run::run(&client, render, &args, services, err),
        Action::Native(args) => leases::native(&client, render, &args, out),
    }
}
