//! The program's stop requests: SIGTERM and SIGINT (and SIGHUP for the cordon verb), read on a
//! thread of their own and handed to the loop through a channel, which is also how the loop
//! waits.

use std::io;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use std::time::Duration;

use tokio::runtime::Builder;
use tokio::signal::unix::{Signal, SignalKind, signal};

use crate::run::{Sleeper, Wake};

/// A [`Sleeper`] that waits on the channel the signal thread sends on, so a stop request
/// wakes it at once and one that came earlier is not lost.
#[derive(Debug)]
pub struct SystemSleeper {
    stops: Receiver<()>,
    /// Kept so the channel never closes (a closed channel would end every wait at once), and
    /// for the handlers [`SystemSleeper::stop_on_hangup`] adds.
    sender: Sender<()>,
}

impl SystemSleeper {
    /// Installs handlers for SIGTERM and SIGINT. They stay installed for the life of the
    /// process, which is the life of the agent.
    ///
    /// # Errors
    ///
    /// The handlers or the thread that reads them cannot be set up.
    pub fn install() -> io::Result<Self> {
        let (sender, stops) = channel();
        watch([SignalKind::terminate(), SignalKind::interrupt()], &sender)
            .map(|()| Self { stops, sender })
    }

    /// Also takes SIGHUP, the end of an ssh session, as a stop request. `js-agent cordon`
    /// asks for it, so that a cordon is lifted when its terminal goes away; the agent daemon
    /// never does, and so does what it always did on SIGHUP.
    ///
    /// # Errors
    ///
    /// The handler or the thread that reads it cannot be set up.
    pub fn stop_on_hangup(&self) -> io::Result<()> {
        watch([SignalKind::hangup()], &self.sender)
    }
}

/// Reads `kinds` of signal on a thread of its own, for the life of the process, and sends a
/// stop request on `to` for each delivery.
fn watch<const N: usize>(kinds: [SignalKind; N], to: &Sender<()>) -> io::Result<()> {
    let forward = to.clone();
    Builder::new_current_thread()
        .enable_all()
        .build()
        .and_then(|runtime| {
            let streams = {
                let _inside = runtime.enter();
                kinds
                    .into_iter()
                    .map(signal)
                    .collect::<io::Result<Vec<_>>>()
            };
            streams.map(|streams| {
                for stream in streams {
                    runtime.spawn(relay(stream, forward.clone()));
                }
                thread::spawn(move || runtime.block_on(std::future::pending::<()>()));
            })
        })
}

/// Sends a stop request for every delivery on `stream`.
async fn relay(mut stream: Signal, to: Sender<()>) {
    loop {
        stream.recv().await;
        let _ = to.send(());
    }
}

impl Sleeper for SystemSleeper {
    fn sleep(&mut self, duration: Duration) -> Wake {
        match self.stops.recv_timeout(duration) {
            Ok(()) => Wake::Stop,
            Err(_) => Wake::Elapsed,
        }
    }
}
