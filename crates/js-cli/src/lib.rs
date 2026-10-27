//! `js`: the command-line client of `js-lease-server`. It acquires, renews and releases
//! leases on whole GPU hosts, and runs a command while it holds one.
//!
//! The library is the program: [`run_with`] takes the arguments, the streams to print
//! to and the [`Services`] (the waiter and the signal source), and `main` calls
//! [`main`], which supplies the program's own.
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
//! | `--ca-cert <path>` | optional: a PEM file with the certificate to trust for `https://`; without it the platform's roots are trusted; ignored for `http://` |
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
//! js lease run  <the flags of acquire> [--renew-every <dur>] -- <command> [args...]
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
//! `run` acquires like `acquire` and starts the command as a child process with
//! `JS_LEASE_ID` and `JS_LEASE_HOST` set in its environment. Those are outputs of `js`
//! to the command; `js` itself reads no environment variable. It renews every
//! `--renew-every` (a third of `--ttl`, or 200s without `--ttl`: a third of the
//! server's default ttl) until the command exits, then releases the lease, whatever
//! happened, and exits with the command's status (128 plus the signal for a command a
//! signal ended). The held-lease line goes to stderr, so stdout is the command's. A
//! SIGINT or SIGTERM sent to `js` is forwarded to the command (from a terminal the
//! command also gets the terminal's own SIGINT), which ends the run the
//! usual way. If a renewal finds the lease gone (404 or 409) `js` sends the command
//! SIGTERM, waits for it to end, and exits 7; a command that ignores SIGTERM is waited
//! for. A renewal that fails for another reason (the server unreachable, say) is
//! reported on stderr and tried again at the next period. A command that cannot be
//! started gives exit 127, and the lease is released.
//!
//! # Exit status
//!
//! | status | meaning |
//! |---|---|
//! | 0 | held, renewed, freed, listed; for `run`, the command's own status |
//! | 1 | the server or the network failed, or the token was refused (401) |
//! | 2 | a flag or a file it names is wrong, including what clap refuses |
//! | 3 | refused: `no-capacity`, `no-fit`, `unsupported-class`, `lease-ended`, `unknown-lease`, `not-held`, `expired`, or a pending start that was dropped (`pending-dropped`) |
//! | 4 | the request itself was refused: a start-contract rule (`unknown-class`, `reserved-label`, `invalid-label`, `below-class-minimum`, `wall-time-not-positive`) or a body the server cannot read |
//! | 5 | a pending start was not confirmed within `--wait-pending` |
//! | 6 | `release`: the stop was accepted but is not confirmed; the server retries it |
//! | 7 | `run`: the lease was lost; the command was stopped |
//! | 127 | `run`: the command could not be started |
//!
//! The reason is on stderr (`js: refused: no-capacity`), and never holds the token.

mod api;
mod cli;
mod client;
mod events;
mod failure;
mod leases;
mod render;
mod run;
mod server_url;
mod signals;
mod token;

use std::ffi::OsString;
use std::io::Write;
use std::process::ExitCode;

use clap::Parser;

use cli::{Action, Cli, Command};
use client::Client;
use failure::Failure;
use render::Render;

pub use events::{ForwardedSignal, Services, SignalSource, Signaller, ThreadWaiter, Waiter};
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
    let client = Client::connect(server, token_file, cli.ca_cert.as_deref())?;
    let Command::Lease(action) = cli.command;
    match action {
        Action::Acquire(args) => leases::acquire(&client, render, &args, &*services.waiter, out),
        Action::Renew(args) => leases::renew(&client, render, &args, out),
        Action::Release(args) => leases::release(&client, render, &args, out),
        Action::List => leases::list(&client, render, out),
        Action::Run(args) => run::run(&client, render, &args, services, err),
    }
}
