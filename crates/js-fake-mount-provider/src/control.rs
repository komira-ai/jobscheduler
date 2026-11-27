//! The control socket: how a test sets the fake's behaviour and reads what it answered.
//!
//! A Unix socket; each connection sends one command line and reads the reply, after which the
//! fake closes it. The commands:
//!
//! | Command | Reply | Effect |
//! |---|---|---|
//! | `reset <limit> <prepare\|mount>` | `ok` | forgets every source and call, serves, and meets the placement limit `<limit>` at `Prepare` or at `Mount` |
//! | `stall` | `ok` | a `Mount` that would place waits, then answers `UNAVAILABLE` "the tree is not ready" |
//! | `serve` | `ok` | serves again; the `Mount` calls waiting are asked again |
//! | `unavailable` | `ok` | every call answers `UNAVAILABLE` |
//! | `restart` | `ok` | closes the provider socket, waits the restart pause, forgets every source, binds again |
//! | `calls` | one line per call answered: `<ms since start> <method> <answer>` | none |
//! | `waiting` | the number of `Mount` calls the stall holds now | none |
//! | `quit` | `ok` | closes both sockets and ends the program |
//!
//! Anything else is answered `error: ...` and changes nothing.

use std::io;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::watch;

use crate::model::{Mode, RefuseAt};
use crate::serve::{Shared, Signal};

/// The reply to `line`, and the change it made.
pub(crate) fn command(line: &str, shared: &Shared, signal: &watch::Sender<Signal>) -> String {
    let words: Vec<&str> = line.split_whitespace().collect();
    let mode = |mode: Mode| {
        shared.store().set_mode(mode);
        shared.changed();
        "ok\n".to_owned()
    };
    match words.as_slice() {
        ["reset", limit, at] => {
            let refuse_at = match *at {
                "prepare" => Some(RefuseAt::Prepare),
                "mount" => Some(RefuseAt::Mount),
                _ => None,
            };
            match (limit.parse::<usize>(), refuse_at) {
                (Ok(limit), Some(refuse_at)) => {
                    shared.store().reset(limit, refuse_at);
                    shared.changed();
                    "ok\n".to_owned()
                }
                _ => format!("error: reset takes a count and prepare or mount: {line:?}\n"),
            }
        }
        ["stall"] => mode(Mode::Stalled),
        ["serve"] => mode(Mode::Serving),
        ["unavailable"] => mode(Mode::Unavailable),
        ["restart"] => {
            signal.send_modify(|now| {
                if let Signal::Run(restarts) = now {
                    *restarts += 1;
                }
            });
            "ok\n".to_owned()
        }
        ["calls"] => shared.store().calls(),
        ["waiting"] => format!("{}\n", shared.waiting()),
        ["quit"] => {
            signal.send_replace(Signal::Quit);
            shared.changed();
            "ok\n".to_owned()
        }
        _ => format!("error: unknown command {line:?}\n"),
    }
}

/// Reads the command on `stream` and writes its reply.
async fn answer(
    stream: UnixStream,
    shared: &Shared,
    signal: &watch::Sender<Signal>,
) -> io::Result<()> {
    let mut stream = BufReader::new(stream);
    let mut line = String::new();
    stream.read_line(&mut line).await?;
    let reply = command(line.trim(), shared, signal);
    stream.get_mut().write_all(reply.as_bytes()).await?;
    stream.get_mut().shutdown().await
}

/// Answers the commands sent on `listener` until one is `quit`. A connection that fails
/// (its client went away) ends only itself.
pub(crate) async fn control(
    listener: UnixListener,
    shared: Arc<Shared>,
    signal: watch::Sender<Signal>,
) -> io::Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        let _ = answer(stream, &shared, &signal).await;
        if *signal.borrow() == Signal::Quit {
            return Ok(());
        }
    }
}
