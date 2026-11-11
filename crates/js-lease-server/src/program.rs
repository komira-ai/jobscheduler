//! The program's run of a server: its own runtime, the line that says where it listens,
//! and the signals and events that end it.

use std::io::Write;

use js_system_clock::SystemClock;
use tokio::runtime::Builder;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;

use crate::error::Failure;
use crate::server::stop_or_fence;
use crate::{Config, IntervalTicker, SharedClock, start};

/// Serves `config` on a runtime of its own until a termination signal, or until the server
/// stops on its own because the store's writer lock was lost or the state store has a newer
/// writer (an error either way: exit status 1).
pub(crate) fn serve(config: Config, out: &mut dyn Write) -> Result<(), Failure> {
    Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(Failure::Runtime)
        .and_then(|runtime| runtime.block_on(serve_until_signal(config, out)))
}

async fn serve_until_signal(mut config: Config, out: &mut dyn Write) -> Result<(), Failure> {
    // The subscriber is global; a second server in one process keeps the first's.
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .try_init();
    let ticker = IntervalTicker::new(config.tick_period());
    // Before the address is printed, so a SIGHUP sent once it is known is not lost.
    let mut hangup = signal(SignalKind::hangup()).map_err(Failure::Runtime)?;
    // Before the readiness address is printed, so a SIGTERM sent once it is known is
    // handled, not left to kill the process.
    let mut termination = Termination::register().map_err(Failure::Runtime)?;
    // Bound before the start, which can wait for the store's writer lock for as long as
    // another process holds it: the address says where the readiness route answers meanwhile.
    if let Some(ready) = config.bind_ready()? {
        let _ = writeln!(out, "ready on {ready}");
        let _ = out.flush();
    }
    // A stop signalled while the start waits ends the process, with the lock not taken.
    let running = tokio::select! {
        started = start(config, SharedClock::new(SystemClock), ticker) => started?,
        () = termination.arrived() => return Ok(()),
    };
    let _ = writeln!(out, "listening on {}", running.addr());
    let _ = out.flush();
    {
        let signalled = async {
            termination.arrived().await;
            Ok(())
        };
        let mut stopping = std::pin::pin!(stop_or_fence(&running, signalled));
        loop {
            tokio::select! {
                stopped = &mut stopping => {
                    stopped.map_err(Failure::Runtime)?;
                    break;
                }
                _ = hangup.recv() => {
                    // The refusal is logged by the reload; the old pair stays.
                    let _ = running.reload_tls();
                }
                // The server has stopped serving and ticking; the shutdown answers the loss.
                _ = running.lock_lost() => break,
            }
        }
    }
    running.shutdown().await.map_err(Failure::Runtime)
}

/// SIGTERM and SIGINT, registered when it is made.
struct Termination {
    received: mpsc::Receiver<()>,
}

impl Termination {
    /// Registers both signals now: from here on neither ends the process by its default
    /// action.
    fn register() -> std::io::Result<Self> {
        let (sender, received) = mpsc::channel(2);
        for kind in [SignalKind::terminate(), SignalKind::interrupt()] {
            let mut stream = signal(kind)?;
            let sender = sender.clone();
            tokio::spawn(async move {
                stream.recv().await;
                let _ = sender.send(()).await;
            });
        }
        Ok(Self { received })
    }

    /// Completes at the first of the two.
    async fn arrived(&mut self) {
        self.received.recv().await;
    }
}
