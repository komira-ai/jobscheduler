//! The program's stop requests: SIGTERM and SIGINT, read on a thread of their own and
//! handed to the loop through a channel, which is also how the loop waits.

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
    /// Kept so the channel never closes: a closed channel would end every wait at once.
    _sender: Sender<()>,
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
        let forward = sender.clone();
        Builder::new_current_thread()
            .enable_all()
            .build()
            .and_then(|runtime| {
                let streams = {
                    let _inside = runtime.enter();
                    [SignalKind::terminate(), SignalKind::interrupt()]
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
            .map(|()| Self {
                stops,
                _sender: sender,
            })
    }
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
