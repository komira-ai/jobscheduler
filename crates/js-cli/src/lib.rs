//! `js`: the command-line client of `js-lease-server`. It acquires, renews and releases
//! leases on whole GPU hosts, and runs a command while it holds one.
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
//! | `--ca-cert <path>` | required for `https://`: a PEM file with the certificate to trust (the server's, or the authority that issued it; a system bundle such as `/etc/ssl/certs/ca-certificates.crt` works). The platform's roots are never read on their own, because the loaders find them through environment variables. Ignored for `http://` |
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
//!                  [--wait-pending <dur>]
//! js lease renew <id> [--ttl <dur>]
//! js lease release <id>
//! js lease list
//! js lease run  <the flags of acquire> [--renew-every <dur>] [--stop-grace <dur>]
//!               -- <command> [args...]
//! ```
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
//! before trying again (the JSON kind is `acquire-unconfirmed`). `acquire` does no more.
//!
//! `run` acquires like `acquire`, with one more label, `js-run=<128 random bits in hex>`,
//! and starts the command as a child process with `JS_LEASE_ID` and `JS_LEASE_HOST` set
//! in its environment. Those are outputs of `js` to the command; `js` itself reads no
//! environment variable. When the answer to its start is lost, `run` lists the leases,
//! releases the one that has its label, says so on stderr and exits 1 as `acquire` does
//! (once: it does not retry the list, so a start the server only finishes afterwards
//! stays held until its ttl).
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
//! | 1 | the server or the network failed, or the token was refused (401); a start whose answer was lost (the lease may exist: run `js lease list`) |
//! | 2 | a flag or a file it names is wrong, including what clap refuses |
//! | 3 | refused: `no-capacity`, `no-fit`, `unsupported-class`, `lease-ended`, `no-capable-host`, `exclusive-not-ready`, `unknown-lease`, `not-held`, `expired`, or a pending start that was dropped (`pending-dropped`) |
//! | 4 | the request itself was refused: a start-contract rule (`unknown-class`, `reserved-label`, `invalid-label`, `below-class-minimum`, `wall-time-not-positive`) or a body the server cannot read |
//! | 5 | a pending start was not confirmed within `--wait-pending` |
//! | 6 | `release`: the stop was accepted but is not confirmed; the server retries it |
//! | 7 | `run`: the lease was lost (gone from the server, or not renewed before the deadline short of its expiry); the command was stopped |
//! | 8 | `run`: the release of the lease could not be confirmed after three tries, so it may stay held until its ttl; replaces the command's own status, which is said on stderr |
//! | 127 | `run`: the command could not be started |
//! | 129, 130, 143 | `run`: SIGHUP, SIGINT or SIGTERM ended `js` while the start was pending (after the command started, the command's own status) |
//!
//! The reason is on stderr (`js: refused: no-capacity`), and never holds the token.

mod api;
mod cli;
mod command;
mod events;
mod failure;
mod leases;
mod render;
mod renewals;
mod run;
mod signals;
mod tag;

use std::ffi::OsString;
use std::io::Write;
use std::process::ExitCode;

use clap::Parser;

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
    match Cli::try_parse_from(args) {
        Ok(cli) => execute(cli, out, err, services),
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

/// Runs a parsed command line and prints its failure, if any.
fn execute(cli: Cli, out: &mut dyn Write, err: &mut dyn Write, services: Services) -> u8 {
    let render = Render::new(cli.json);
    match dispatch(cli, render, out, err, services) {
        Ok(code) => code,
        Err(failure) => {
            let _ = err.write_all(render.failure(&failure).as_bytes());
            failure.parts().0
        }
    }
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
    }
}
