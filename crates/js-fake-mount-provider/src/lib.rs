//! `js-fake-mount-provider`: a node-local mount provider for black-box tests of the released
//! js-agent, run as a program of its own beside it.
//!
//! It serves `jobscheduler.provider.v1.MountProvider`, the service tierstore serves the agent,
//! on a Unix socket, and answers as tierstore does where the agent can see it (a `Prepare`
//! answered at once, the tree placed at `Mount`, a placement limit, a store that stalls or is
//! unavailable, a restart that forgets the sources), so a test of the agent needs no tierstore.
//! A second socket, the control socket, sets that behaviour and reads back every call it
//! answered (see the module `control`). Every tree it places is the same: a directory
//! with one empty file, `input/ready`.
//!
//! ```text
//! js-fake-mount-provider --socket PATH --control PATH --root DIR
//!     [--max-placements N] [--refuse-at mount|prepare]
//!     [--mount-wait-ms N] [--restart-pause-ms N]
//! ```
//!
//! - `--socket`: the provider socket, made mode 0600 (the agent's `--mount-provider`).
//! - `--control`: the control socket.
//! - `--root`: the directory the trees are placed under, an absolute path (the agent's
//!   `--mount-root-ro` of the provider).
//! - `--max-placements` (default 1024, tierstore's): the placements alive at once.
//! - `--refuse-at` (default `mount`, as tierstore answers): where the limit is met.
//! - `--mount-wait-ms` (default 25000, tierstore's): how long a `Mount` waits on a stalled
//!   store before it is answered not ready.
//! - `--restart-pause-ms` (default 1000): how long the provider socket is closed by a restart.
//!
//! It prints `serving <socket>` once both sockets are bound, and ends with status 0 after a
//! `quit` on the control socket, 1 when a socket cannot be served, and 2 on a usage error.

mod control;
mod model;
mod serve;

use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{oneshot, watch};

use model::{RefuseAt, Store};
use serve::{Shared, Signal};

/// What the flags say.
#[derive(Debug, PartialEq, Eq)]
struct Config {
    socket: PathBuf,
    control: PathBuf,
    root: PathBuf,
    limit: usize,
    refuse_at: RefuseAt,
    wait: Duration,
    pause: Duration,
}

/// The configuration the flags `args` give, or why they give none.
fn parse(args: Vec<OsString>) -> Result<Config, String> {
    let (mut socket, mut control, mut root) = (None, None, None);
    let (mut limit, mut refuse_at) = (1024, RefuseAt::Mount);
    let (mut wait, mut pause) = (Duration::from_millis(25_000), Duration::from_millis(1_000));
    let mut args = args.into_iter();
    while let Some(flag) = args.next() {
        let flag = flag.to_string_lossy().into_owned();
        let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
        let text = value.to_string_lossy().into_owned();
        let count = || {
            text.parse::<u64>()
                .map_err(|_| format!("{flag}: {text:?} is not a count"))
        };
        match flag.as_str() {
            "--socket" => socket = Some(PathBuf::from(value)),
            "--control" => control = Some(PathBuf::from(value)),
            "--root" => root = Some(PathBuf::from(value)),
            "--max-placements" => limit = usize::try_from(count()?).unwrap_or(usize::MAX),
            "--refuse-at" => {
                refuse_at = match text.as_str() {
                    "mount" => RefuseAt::Mount,
                    "prepare" => RefuseAt::Prepare,
                    _ => return Err(format!("--refuse-at: {text:?} is not mount or prepare")),
                }
            }
            "--mount-wait-ms" => wait = Duration::from_millis(count()?),
            "--restart-pause-ms" => pause = Duration::from_millis(count()?),
            _ => return Err(format!("{flag}: not a flag of js-fake-mount-provider")),
        }
    }
    let root = root.ok_or("--root is required")?;
    if !root.is_absolute() {
        return Err(format!(
            "--root: {} is not an absolute path",
            root.display()
        ));
    }
    Ok(Config {
        socket: socket.ok_or("--socket is required")?,
        control: control.ok_or("--control is required")?,
        root,
        limit,
        refuse_at,
        wait,
        pause,
    })
}

/// Serves `config` until a `quit` on the control socket.
async fn run(config: Config) -> io::Result<()> {
    let store = Store::new(config.limit, config.refuse_at);
    let shared = Shared::new(store, config.root, config.wait);
    let (signal, signals) = watch::channel(Signal::Run(0));
    let _ = std::fs::remove_file(&config.control);
    let listener = tokio::net::UnixListener::bind(&config.control)?;
    let (bound, bound_now) = oneshot::channel();
    let socket = config.socket.clone();
    let shared_too = Arc::clone(&shared);
    let served = tokio::spawn(serve::serve(
        shared_too,
        socket,
        config.pause,
        signals,
        bound,
    ));
    if bound_now.await.is_err() {
        // The provider socket was not bound: the serve's error says why.
        return served.await.map_err(io::Error::other)?;
    }
    println!("serving {}", config.socket.display());
    let controlled = control::control(listener, shared, signal).await;
    let served = served.await.map_err(io::Error::other)?;
    let _ = std::fs::remove_file(&config.control);
    served.and(controlled)
}

/// The program: serves the flags `args` (the arguments after the program's name) give, and
/// returns its exit status.
pub fn main(args: impl IntoIterator<Item = OsString>) -> ExitCode {
    let config = match parse(args.into_iter().collect()) {
        Ok(config) => config,
        Err(usage) => {
            eprintln!("js-fake-mount-provider: {usage}");
            return ExitCode::from(2);
        }
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build();
    let ended = runtime.and_then(|runtime| runtime.block_on(run(config)));
    match ended {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("js-fake-mount-provider: {e}");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
