//! The [`Wake`] the worker gives its lifecycle: the work handed off the worker (a pull, a
//! lookup or an assembly, a provider's call) rings it when it is done, and the worker steps
//! at once, so a start that waited on it goes on within that step instead of the next step
//! interval.
//!
//! A ring puts at most one nudge in the queue: a flag set by the ring that sends it and
//! cleared by the worker when it takes any order (every order is followed by a step, which
//! sees what was done before it). So rings never crowd replies out of the queue. A ring
//! that finds the queue full sends nothing and clears the flag: the orders in the queue each
//! bring a step. The wake holds the queue weakly, so a worker whose handles are all gone
//! ends as it did before (its queue disconnects), whatever the lifecycle still holds.

use std::sync::atomic::Ordering;
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Weak};

use super::{Order, Shared};
use crate::lifecycle::Wake;

/// The wake of the worker whose queue is `orders` and whose flags are `shared`.
pub(super) fn wake(orders: &Arc<SyncSender<Order>>, shared: &Arc<Shared>) -> Wake {
    let (orders, shared) = (Arc::downgrade(orders), Arc::clone(shared));
    Wake::new(move || ring(&orders, &shared))
}

/// Sends a nudge unless one waits in the queue already.
fn ring(orders: &Weak<SyncSender<Order>>, shared: &Shared) {
    if shared.nudge_queued.swap(true, Ordering::SeqCst) {
        return;
    }
    let sent = orders
        .upgrade()
        .is_some_and(|orders| orders.try_send(Order::Nudge).is_ok());
    if !sent {
        shared.nudge_queued.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::{TryRecvError, sync_channel};

    use super::*;

    /// What the queue holds now, as names.
    fn queued(inbox: &std::sync::mpsc::Receiver<Order>) -> Vec<&'static str> {
        let mut names = Vec::new();
        loop {
            match inbox.try_recv() {
                Ok(Order::Nudge) => names.push("nudge"),
                Ok(_) => names.push("other"),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return names,
            }
        }
    }

    #[test]
    fn rings_before_the_worker_takes_the_nudge_put_one_nudge_in_the_queue() {
        let (orders, inbox) = sync_channel(4);
        let (orders, shared) = (Arc::new(orders), Arc::new(Shared::default()));
        let wake = wake(&orders, &shared);
        wake.wake();
        wake.wake();
        assert_eq!(queued(&inbox), ["nudge"]);
        // The worker took it (and cleared the flag): the next ring sends another.
        shared.nudge_queued.store(false, Ordering::SeqCst);
        wake.wake();
        assert_eq!(queued(&inbox), ["nudge"]);
    }

    #[test]
    fn a_ring_that_finds_the_queue_full_sends_nothing_and_the_next_ring_tries_again() {
        let (orders, inbox) = sync_channel(1);
        let (orders, shared) = (Arc::new(orders), Arc::new(Shared::default()));
        orders.try_send(Order::Prepare).unwrap();
        let wake = wake(&orders, &shared);
        wake.wake();
        assert!(!shared.nudge_queued.load(Ordering::SeqCst));
        assert_eq!(queued(&inbox), ["other"]);
        wake.wake();
        assert_eq!(queued(&inbox), ["nudge"]);
    }

    #[test]
    fn a_wake_keeps_no_queue_alive_and_rings_nothing_once_the_queue_is_gone() {
        let (orders, inbox) = sync_channel(1);
        let (orders, shared) = (Arc::new(orders), Arc::new(Shared::default()));
        let wake = wake(&orders, &shared);
        drop(orders);
        // The worker's end: every sender is gone although the wake is still held.
        assert_eq!(inbox.try_recv().err(), Some(TryRecvError::Disconnected));
        wake.wake();
        assert!(!shared.nudge_queued.load(Ordering::SeqCst));
    }
}
