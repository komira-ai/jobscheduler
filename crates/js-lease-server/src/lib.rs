//! A lease server: a [`js_lease::LeaseTable`] behind an authenticated HTTP JSON API, so
//! a CI job can hold a whole GPU host for as long as it renews.
//!
//! The library is the program: [`run`] takes the arguments and the streams to report
//! to, and `main` calls it. The pieces:
//!
//! - [`Config`] and [`Cli`]: the flags, and the checks that refuse a start (a token file
//!   anyone can read, a listen address off the loopback without TLS).
//! - [`CapacitySource`]: where the hosts come from. [`StaticPoolSource`] reads the
//!   operator's pool file. Hosts registered by agents are a future source of the same
//!   trait; it is not built here.
//! - [`start`] and [`Running`]: the server, with time and ticks injected ([`SharedClock`],
//!   [`Ticker`]) so a test steps both.
//!
//! # Flags
//!
//! Configuration is flags only; the one secret, the bearer token, is read from a file
//! that only its owner can use.
//!
//! | flag | meaning |
//! |---|---|
//! | `--listen <addr>` | default `127.0.0.1:7300`; off the loopback it needs both TLS flags |
//! | `--state-file <path>` | the lease file |
//! | `--boot initialise\|resume` | required: `initialise` may create the file, `resume` needs it |
//! | `--pool-config <path>` | the static pool file ([`js_pool_static`]) |
//! | `--token-file <path>` | required, mode 0600 or 0400 |
//! | `--default-ttl`, `--max-ttl` | `600s` and `3600s` unless given; `90`, `90s`, `15m`, `2h` |
//! | `--tick` | how often the table is ticked, `5s` unless given |
//! | `--tls-cert`, `--tls-key` | PEM files; both or neither |
//!
//! A start that is refused prints the flag at fault and exits with status 2.
//!
//! # API
//!
//! Every route but `/healthz` needs `Authorization: Bearer <token>`; a missing or wrong
//! token is a 401 with an empty body. Bodies are JSON, at most 64 KiB. Errors are
//! `{"error": "<code>"}` (a 400 adds `"reason"`).
//!
//! | request | answers |
//! |---|---|
//! | `POST /v1/leases` | 201 `{id, host, expires_at_ms}`; 202 `{id, state: "pending"}` when the provider has not confirmed the start; 409 for `no-capacity`, `no-fit`, `unsupported-class`, `lease-ended`; 422 for a start-contract refusal (`wall-time-not-positive`, `reserved-label`, `invalid-label`, `unknown-class`, `below-class-minimum`) |
//! | `POST /v1/leases/{id}/renew` | 200 `{expires_at_ms}`; 404 `unknown-lease`; 409 `not-held` or `expired` |
//! | `DELETE /v1/leases/{id}` | 200 `{state: "freed"}`; 202 `{state: "releasing"}`; 404 |
//! | `GET /v1/leases` | 200 `{leases: [{id, state, host?, acquired_at_ms, expires_at_ms, ends_at_ms, labels}]}` |
//! | `GET /healthz` | 200 `ok`, no token |
//!
//! An unknown route is a 404, a known route with another method a 405, a body that is
//! not the JSON the route takes a 400, a body over the cap a 413, and a lease file that
//! cannot be written a 500 (`state-write-failed`; the cause is in the log).
//!
//! The table sits behind one async mutex, locked for the table call alone and never
//! across an await.
//!
//! # Ticks
//!
//! Only a tick expires a lease, retries a stop or resolves a pending start. The server
//! ticks the table at every tick of its [`Ticker`] (a timer of `--tick` in production),
//! logs what a tick changed, and counts the ticks that could not write the lease file
//! ([`Running::save_errors`]). On SIGTERM or ctrl-c the server finishes the requests in
//! flight, ticks once more and exits 0.

mod adapt;
mod api;
mod capacity;
mod config;
mod error;
mod listener;
mod server;
mod ticker;
mod tls;
mod util;
mod wire;

use std::ffi::OsString;
use std::io::Write;
use std::process::ExitCode;

use clap::Parser;
use js_lease::SystemClock;
use tokio::runtime::Builder;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;

pub use adapt::SharedClock;
pub use api::BODY_LIMIT;
pub use capacity::{CapacitySource, StaticPoolSource};
pub use config::{BootMode, Cli, Config, TlsFiles, Token, parse_duration};
pub use error::StartupError;
pub use server::{Running, start};
pub use ticker::{ChannelTicker, IntervalTicker, Tick, TickHandle, Ticker, channel_ticker};

use error::Failure;

/// The program: runs with the process's arguments and standard streams.
pub fn main() -> ExitCode {
    run(
        std::env::args_os(),
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    )
}

/// Runs the server as the command line `args` (the program name first) says, until
/// SIGTERM or ctrl-c. Usage text and the line naming the listen address go to `out`;
/// a refusal or failure goes to `err`.
///
/// Exit status: 0 after an orderly stop (and for `--help`), 2 for a usage error or a
/// start that was refused (the message names the flag), 1 for any other failure.
pub fn run(
    args: impl IntoIterator<Item = OsString>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let parsed = Cli::try_parse_from(args);
    let cli = match parsed {
        Ok(cli) => cli,
        Err(usage) => return usage_exit(&usage, out, err),
    };
    let result = Config::from_cli(cli)
        .map_err(Failure::from)
        .and_then(|config| serve(config, out));
    exit_code(result, err)
}

/// Prints a usage error where clap would and picks the exit status clap picks.
fn usage_exit(usage: &clap::Error, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    let _ = match usage.use_stderr() {
        true => write!(err, "{usage}"),
        false => write!(out, "{usage}"),
    };
    ExitCode::from(u8::try_from(usage.exit_code()).unwrap_or(2))
}

/// Maps a result to the exit status, printing a failure to `err`.
fn exit_code(result: Result<(), Failure>, err: &mut dyn Write) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => {
            let _ = writeln!(err, "js-lease-server: {failure}");
            failure.code()
        }
    }
}

/// Serves `config` on a runtime of its own until a termination signal.
fn serve(config: Config, out: &mut dyn Write) -> Result<(), Failure> {
    Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(Failure::Runtime)
        .and_then(|runtime| runtime.block_on(serve_until_signal(config, out)))
}

async fn serve_until_signal(config: Config, out: &mut dyn Write) -> Result<(), Failure> {
    // The subscriber is global; a second server in one process keeps the first's.
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .try_init();
    let ticker = IntervalTicker::new(config.tick_period());
    let running = start(config, SharedClock::new(SystemClock), ticker).await?;
    let _ = writeln!(out, "listening on {}", running.addr());
    let _ = out.flush();
    termination().await.map_err(Failure::Runtime)?;
    running.shutdown().await.map_err(Failure::Runtime)
}

/// Completes at SIGTERM or SIGINT.
async fn termination() -> std::io::Result<()> {
    let (sender, mut received) = mpsc::channel(2);
    for kind in [SignalKind::terminate(), SignalKind::interrupt()] {
        let mut stream = signal(kind)?;
        let sender = sender.clone();
        tokio::spawn(async move {
            stream.recv().await;
            let _ = sender.send(()).await;
        });
    }
    received.recv().await;
    Ok(())
}
