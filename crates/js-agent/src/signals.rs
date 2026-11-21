//! The program's requests: stop (SIGTERM and SIGINT, and SIGHUP for the cordon verb) and replace
//! (SIGHUP, for the agent daemon), read on a thread of their own and handed to the loop through
//! a line it waits on, which also carries the job worker's bell.

use std::io;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

use tokio::runtime::Builder;
use tokio::signal::unix::{Signal, SignalKind, signal};

use crate::run::{Sleeper, Wake};

/// What waits on the line for the loop: the stop and replace requests not yet taken, and
/// whether the bell rang since a wait last took it.
#[derive(Debug, Default)]
struct Calls {
    stops: u64,
    replaces: u64,
    bell: bool,
}

/// The line the signal thread and the bell call the loop on.
#[derive(Debug, Default)]
struct Line {
    calls: Mutex<Calls>,
    changed: Condvar,
}

impl Line {
    fn lock(&self) -> MutexGuard<'_, Calls> {
        self.calls.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Changes what waits on the line with `call`, and wakes the loop.
    fn call(&self, call: impl FnOnce(&mut Calls)) {
        call(&mut self.lock());
        self.changed.notify_all();
    }

    /// Waits `duration`, or until a stop or replace request is there, or the bell when `bell`
    /// (see [`Sleeper::sleep_or_bell`]). A stop request is taken first, then a replace
    /// request, then the bell.
    fn wait(&self, duration: Duration, bell: bool) -> Wake {
        let waits =
            |calls: &mut Calls| calls.stops == 0 && calls.replaces == 0 && !(bell && calls.bell);
        let calls = self.lock();
        let (mut calls, _) = self
            .changed
            .wait_timeout_while(calls, duration, waits)
            .unwrap_or_else(PoisonError::into_inner);
        if calls.stops > 0 {
            calls.stops -= 1;
            Wake::Stop
        } else if calls.replaces > 0 {
            calls.replaces -= 1;
            Wake::Replace
        } else if bell && calls.bell {
            calls.bell = false;
            Wake::Bell
        } else {
            Wake::Elapsed
        }
    }
}

/// A [`Sleeper`] that waits on the line the signal thread calls on, so a stop request
/// wakes it at once and one that came earlier is not lost; and the job worker's bell, which
/// ends a wait that may end early ([`Sleeper::sleep_or_bell`]).
#[derive(Debug)]
pub struct SystemSleeper {
    line: Arc<Line>,
}

impl SystemSleeper {
    /// Installs handlers for SIGTERM and SIGINT. They stay installed for the life of the
    /// process, which is the life of the agent.
    ///
    /// # Errors
    ///
    /// The handlers or the thread that reads them cannot be set up.
    pub fn install() -> io::Result<Self> {
        let sleeper = Self {
            line: Arc::default(),
        };
        let kinds = [SignalKind::terminate(), SignalKind::interrupt()];
        watch(kinds, &sleeper.line, Request::Stop).map(|()| sleeper)
    }

    /// Also takes SIGHUP, the end of an ssh session, as a stop request. `js-agent cordon`
    /// asks for it, so that a cordon is lifted when its terminal goes away; the agent daemon
    /// never does, and so does what it always did on SIGHUP.
    ///
    /// # Errors
    ///
    /// The handler or the thread that reads it cannot be set up.
    pub fn stop_on_hangup(&self) -> io::Result<()> {
        watch([SignalKind::hangup()], &self.line, Request::Stop)
    }

    /// Takes SIGHUP as a request to hand the runs over and run the program again in place
    /// ([`Wake::Replace`]): the update hook sends it to the main process, after it wrote the
    /// handover file, instead of stopping the unit. Only the agent daemon asks for it.
    ///
    /// # Errors
    ///
    /// The handler or the thread that reads it cannot be set up.
    pub fn replace_on_hangup(&self) -> io::Result<()> {
        watch([SignalKind::hangup()], &self.line, Request::Replace)
    }
}

/// What a delivery of a signal asks the loop for.
#[derive(Debug, Clone, Copy)]
enum Request {
    Stop,
    Replace,
}

/// Reads `kinds` of signal on a thread of its own, for the life of the process, and puts a
/// `request` on `line` for each delivery.
fn watch<const N: usize>(
    kinds: [SignalKind; N],
    line: &Arc<Line>,
    request: Request,
) -> io::Result<()> {
    let line = Arc::clone(line);
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
                    runtime.spawn(relay(stream, Arc::clone(&line), request));
                }
                thread::spawn(move || runtime.block_on(std::future::pending::<()>()));
            })
        })
}

/// Puts `request` on the line for every delivery on `stream`.
async fn relay(mut stream: Signal, to: Arc<Line>, request: Request) {
    loop {
        stream.recv().await;
        to.call(|calls| match request {
            Request::Stop => calls.stops += 1,
            Request::Replace => calls.replaces += 1,
        });
    }
}

impl Sleeper for SystemSleeper {
    fn sleep(&mut self, duration: Duration) -> Wake {
        self.line.wait(duration, false)
    }

    fn sleep_or_bell(&mut self, duration: Duration) -> Wake {
        self.line.wait(duration, true)
    }

    fn bell(&self) -> Option<crate::lifecycle::Wake> {
        let line = Arc::clone(&self.line);
        let ring = move || line.call(|calls| calls.bell = true);
        Some(crate::lifecycle::Wake::new(ring))
    }
}

#[cfg(test)]
#[path = "signals_tests.rs"]
mod tests;
