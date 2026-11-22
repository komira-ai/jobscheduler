//! The placement poller: the task that runs the server's pass over the queued jobs each time it
//! is poked. It is the only place an event asks for placement; the tick's forced pass
//! ([`Agents::place_jobs`]) beside it is the net for what only the clock changes.
//!
//! - A submit commits its job as queued and pokes it ([`Poller::poke`]); the submit's answer is
//!   the queued job, never a placement. The pass that poke brings is the forced one
//!   ([`Agents::place_jobs`]).
//! - A heartbeat of a host that lists its jobs pokes it after the listing's commit
//!   ([`Poller::poke_after_beat`]). The pass that poke brings, when no submit poked too, is the
//!   heartbeat's ([`Agents::place_jobs_after_beat`]): skipped when nothing it depends on changed
//!   since the last pass, so a quiet fleet's beats cost no pass.
//!
//! A poke is a [`Notify`] permit: pokes that arrive while a pass runs, or before the task first
//! waits, are kept as one, so the task runs one more pass after the last poke, and that pass
//! reads every commit made before it; it is forced when any of the pokes it answers was a
//! submit's. Pokes never queue up behind each other: however many submits and heartbeats come
//! while a pass runs, one pass follows it.
//!
//! The pass is the one the tick runs: it claims each host's room in the store's own transaction,
//! wakes the held heartbeat or dispatch poll of each host it places a job on (whose reply then
//! carries the start), and returns the hosts its host jobs tainted, whose bindings the poller
//! writes ([`Agents::keep_taints`]) before it waits again. One pass of the server runs at a time
//! whoever runs it ([`Agents::placing`]). A server that drains runs no pass, poked or not.
//!
//! The server runs [`pass`] in a loop on a task it spawns with its tick loop ([`crate::server`])
//! and ends with it. The server in process ([`crate::in_process`]) spawns no task: its caller
//! runs the poller's turn itself ([`poked_pass`], [`crate::in_process::InProcess::poll`]) after
//! each request, and a poke it does not take waits for the next tick, whose pass places the
//! job.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::Notify;

use crate::agents::Agents;
use crate::util::blocking;

/// The wake signal of the placement poller.
#[derive(Default)]
pub(crate) struct Poller {
    /// The poke: one permit, kept until the poller takes it.
    poked: Notify,
    /// Whether a poke the poller has not answered yet asks for the forced pass (a submit's).
    forced: AtomicBool,
}

impl Poller {
    /// Asks the poller for the forced pass over the queued jobs, as a submit does. Returns at
    /// once: the pass runs on the poller's task.
    pub(crate) fn poke(&self) {
        self.forced.store(true, Ordering::SeqCst);
        self.poked.notify_one();
    }

    /// Asks the poller for the heartbeat's pass, which runs only when something it depends on
    /// changed since the last pass. Returns at once.
    pub(crate) fn poke_after_beat(&self) {
        self.poked.notify_one();
    }

    /// Whether a poke is waiting, taking it if so: what the poller's next wait would see at
    /// once. The server in process drives its poller by it ([`poked_pass`]); a test checks by
    /// it that a poke came, or did not, without running a pass.
    #[cfg(any(test, feature = "in-process"))]
    pub(crate) fn take_poke(&self) -> bool {
        let waiting = std::pin::pin!(self.poked.notified());
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        std::future::Future::poll(waiting, &mut cx).is_ready()
    }
}

/// One turn of the poller of `agents`: waits for a poke, runs a pass over the queued jobs on the
/// pool for blocking calls (the forced one when a submit poked, the heartbeat's otherwise) and
/// keeps the taints of the hosts its host jobs were placed on: [`poked`], then [`run`], which
/// the server's poller ([`crate::server`]) runs in a loop with its stop checked between them.
pub(crate) async fn pass(agents: &Arc<Agents>) {
    poked(agents).await;
    run(agents).await;
}

/// The poller's wait: until a poke comes, or at once when one is waiting.
pub(crate) async fn poked(agents: &Arc<Agents>) {
    agents.poller.poked.notified().await;
}

/// The poller's pass, once poked: the forced one when a submit poked, the heartbeat's
/// otherwise, on the pool for blocking calls, then the taints of the hosts its host jobs were
/// placed on.
pub(crate) async fn run(agents: &Arc<Agents>) {
    // Taken after the wake: a submit's poke that comes after this read brings a permit of its
    // own, so its forced pass follows this one.
    let forced = agents.poller.forced.swap(false, Ordering::SeqCst);
    let placing = Arc::clone(agents);
    let tainted = blocking(move || match forced {
        true => placing.place_jobs(),
        false => placing.place_jobs_after_beat(),
    })
    .await;
    agents.keep_taints(tainted).await;
}

/// The poller's turn when a poke is waiting: takes it and runs [`pass`]'s pass over the queued
/// jobs, and says whether it did; nothing when no poke is waiting. What the server in process
/// runs in place of the poller's task.
#[cfg(any(test, feature = "in-process"))]
pub(crate) async fn poked_pass(agents: &Arc<Agents>) -> bool {
    let poked = agents.poller.take_poke();
    if poked {
        // The poke just taken is given back for `pass`, whose wait then ends at once; whether
        // it asks for the forced pass is kept as it was.
        agents.poller.poked.notify_one();
        pass(agents).await;
    }
    poked
}

#[cfg(test)]
#[path = "poller_tests.rs"]
mod tests;
