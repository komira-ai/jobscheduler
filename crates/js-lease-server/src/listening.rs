//! What the other servers of a shared store tell this one (`docs/design/multi-instance.md`,
//! section 0 and G16): the notices their job stores send after a commit ([`js_jobs::Wake`]),
//! heard on a connection of this server's own ([`Listener`]) and turned into the wakes this
//! server already has for its own writes.
//!
//! - **The queue** pokes the placement poller ([`crate::poller::Poller::poke_after_beat`]): a
//!   job another server queued, an allocation it freed or a cancel it took is placed now, not at
//!   the next tick. The pass reads the store's head first and is skipped when nothing it reads
//!   changed, so a notice for something it has seen costs a read.
//! - **A host** wakes the host's held heartbeat ([`crate::held_beat`]) and held dispatch poll
//!   ([`crate::dispatch_state`]), as a placement of this server's own does: a start or stop
//!   another server wrote for the host goes out now, not at the poll's recheck or the beat's
//!   bound.
//! - **The connection made**, the first time or again, wakes all of them once: a notice sent
//!   while it was not made was missed.
//!
//! A notice only ends a wait early; the rows decide, and the ticks, rechecks and bounds stay
//! (the store one process writes, SQLite, sends none and has no listener). The listener runs on
//! a thread of its own, since its calls block, until the server that started it is dropped; a
//! lost connection is logged and made again [`RELISTEN`] later.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use js_jobs::Wake;
use js_state::postgres::{Heard, Listener};
use tracing::warn;

use crate::agents::Agents;
use crate::store::ServerStore;

/// How long the listener waits for a notice before it looks whether it is to stop.
const WAIT: Duration = Duration::from_secs(1);

/// How long after a lost connection the listener makes it again: what bounds the attempts, and
/// the lines logged, while the database cannot be reached.
pub(crate) const RELISTEN: Duration = Duration::from_secs(5);

/// The listener of a running server: dropped, it stops at its next look, or at once when it
/// waits to connect again.
pub(crate) struct Listening {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Listening {
    /// Stops the listener and waits for its thread to end. For tests: a server's drop does not
    /// wait for it.
    #[cfg(test)]
    pub(crate) fn join(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let thread = self.thread.take();
        thread.into_iter().for_each(|thread| {
            thread.thread().unpark();
            thread.join().unwrap();
        });
    }
}

impl Drop for Listening {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.thread
            .iter()
            .for_each(|thread| thread.thread().unpark());
    }
}

/// The listener of the server whose agent side is `agents`, over `store` when it is one other
/// servers share; `None` for no store, or one this process alone writes.
pub(crate) fn start(store: Option<&ServerStore>, agents: &Arc<Agents>) -> Option<Listening> {
    let listener = store?.listener()?;
    Some(listen(Arc::clone(agents), listener, RELISTEN))
}

/// Runs `listener` on a thread of its own for `agents` until the answer is dropped, making a lost
/// connection again `relisten` after it was lost.
pub(crate) fn listen(agents: Arc<Agents>, mut listener: Listener, relisten: Duration) -> Listening {
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = Arc::clone(&stop);
    let thread = thread::spawn(move || {
        while !stopped.load(Ordering::SeqCst) {
            let heard = listener.next(WAIT);
            heard_by(&agents, &heard);
            if matches!(heard, Heard::Lost(_)) {
                // Woken early by the drop; a wake for nothing only connects sooner.
                thread::park_timeout(relisten);
            }
        }
    });
    Listening {
        stop,
        thread: Some(thread),
    }
}

/// Wakes what `heard` asks `agents` to look at again (see the module documentation).
pub(crate) fn heard_by(agents: &Agents, heard: &Heard) {
    match heard {
        Heard::Listening => {
            agents.poller.poke_after_beat();
            agents.wakes.wake_all();
            agents.dispatching.wakes.wake_all();
        }
        Heard::Notice(notice) => match Wake::of(notice) {
            Some(Wake::Queue) => agents.poller.poke_after_beat(),
            Some(Wake::Host(host)) => {
                agents.wakes.wake(&host);
                agents.dispatching.wakes.wake(&host);
            }
            None => {}
        },
        Heard::Quiet => {}
        Heard::Lost(error) => {
            warn!(%error, "other servers' notices are not heard until the store's listening connection is made again: their writes are found at the next poll");
        }
    }
}

#[cfg(test)]
#[path = "listening_tests.rs"]
mod tests;
