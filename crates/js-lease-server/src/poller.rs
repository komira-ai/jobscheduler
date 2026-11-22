//! The placement poller: the task that runs the server's pass over the queued jobs
//! ([`Agents::place_jobs`]) each time it is poked. A submit commits its job as queued and pokes
//! it ([`Poller::poke`]); the submit's answer is the queued job, never a placement.
//!
//! A poke is a [`Notify`] permit: pokes that arrive while a pass runs, or before the task first
//! waits, are kept as one, so the task runs one more pass after the last poke, and that pass
//! reads every commit made before it. Pokes never queue up behind each other: however many
//! submits come while a pass runs, one pass follows it.
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

use tokio::sync::Notify;

use crate::agents::Agents;
use crate::util::blocking;

/// The wake signal of the placement poller.
#[derive(Default)]
pub(crate) struct Poller {
    /// The poke: one permit, kept until the poller takes it.
    poked: Notify,
}

impl Poller {
    /// Asks the poller for a pass over the queued jobs. Returns at once: the pass runs on the
    /// poller's task.
    pub(crate) fn poke(&self) {
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
/// pool for blocking calls and keeps the taints of the hosts its host jobs were placed on. The
/// server's poller ([`crate::server`]) runs it in a loop.
pub(crate) async fn pass(agents: &Arc<Agents>) {
    agents.poller.poked.notified().await;
    let placing = Arc::clone(agents);
    let tainted = blocking(move || placing.place_jobs()).await;
    agents.keep_taints(tainted).await;
}

/// The poller's turn when a poke is waiting: takes it and runs [`pass`]'s pass over the queued
/// jobs, and says whether it did; nothing when no poke is waiting. What the server in process
/// runs in place of the poller's task.
#[cfg(any(test, feature = "in-process"))]
pub(crate) async fn poked_pass(agents: &Arc<Agents>) -> bool {
    let poked = agents.poller.take_poke();
    if poked {
        // The poke just taken is given back for `pass`, whose wait then ends at once.
        agents.poller.poke();
        pass(agents).await;
    }
    poked
}

#[cfg(test)]
#[path = "poller_tests.rs"]
mod tests;
