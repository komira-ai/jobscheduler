//! How replies reach the worker's queue, and the room signal of that queue.
//!
//! A reply goes into the bounded queue ([`QUEUE`](super::QUEUE) orders) without waiting
//! ([`Worker::try_deliver`], [`Worker::try_deliver_commands`]): a queue that is full refuses
//! it ([`Delivery::Full`]) and one with no reader any more refuses it as [`Delivery::Gone`].
//!
//! Every order the worker takes from the queue makes room for one more, and the worker
//! counts them ([`Worker::taken`]). [`Worker::wait_room`] waits until that count passes one
//! read before, so a caller that found the queue full (the dispatch poller, which holds the
//! messages it could not hand over) tries again as soon as there is room, not after a fixed
//! wait. The caller reads the count before it tries: an order taken between that read and the
//! refusal ends the wait at once, so no room is missed. A wait also ends when the worker
//! ends.

use std::sync::PoisonError;
use std::sync::atomic::Ordering;
use std::sync::mpsc::TrySendError;
use std::time::Duration;

use js_agent_wire::{Command, HeartbeatReply};

use super::{Order, Shared, Worker};

/// What became of a reply handed to the worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// The reply waits in the queue.
    Delivered,
    /// The worker is busy and the queue is full: the reply was not queued.
    Full,
    /// Nobody reads the queue any more: the worker's thread is gone.
    Gone,
}

impl Worker {
    /// Hands a reply to the worker without waiting: [`Delivery::Full`] when the worker is
    /// busy and the reply was dropped, [`Delivery::Gone`] when nobody reads the queue. The
    /// reply is stamped with the clock now, so a start that waits in the queue has its
    /// `valid_for_ms` counted from the moment it arrived.
    pub fn try_deliver(&self, reply: HeartbeatReply) -> Delivery {
        self.deliver(reply, true, self.clock.now_ms())
    }

    /// [`Worker::try_deliver`] for the commands of a dispatch poll's answer: they are acted
    /// on as a reply's are, and the lease the host holds is left as it is. They are stamped
    /// `received_ms` on the lifecycle's clock ([`Worker::clock`]): when the answer arrived,
    /// which for messages held while the queue was full is earlier than now.
    pub fn try_deliver_commands(&self, commands: Vec<Command>, received_ms: u64) -> Delivery {
        let reply = HeartbeatReply {
            commands,
            ..HeartbeatReply::default()
        };
        self.deliver(reply, false, received_ms)
    }

    fn deliver(&self, reply: HeartbeatReply, whole: bool, received_ms: u64) -> Delivery {
        let reply = Box::new(reply);
        let order = Order::Reply {
            reply,
            received_ms,
            whole,
        };
        match self.handle.orders.try_send(order) {
            Ok(()) => Delivery::Delivered,
            Err(TrySendError::Full(_)) => Delivery::Full,
            Err(TrySendError::Disconnected(_)) => Delivery::Gone,
        }
    }

    /// How many orders the worker has taken from its queue so far. Read it before a
    /// delivery, and give it to [`Worker::wait_room`] when the delivery found the queue full.
    pub fn taken(&self) -> u64 {
        self.handle.shared.taken.load(Ordering::SeqCst)
    }

    /// Waits until the worker has taken more orders than `seen` (a count [`Worker::taken`]
    /// read), and so made room, or has ended; for at most `limit`, a bound for the caller to
    /// look at its own state again, not a time it expects to pass. True when the wait ended
    /// on room or the end, false when `limit` passed.
    pub fn wait_room(&self, seen: u64, limit: Duration) -> bool {
        self.handle.shared.wait_room(seen, limit)
    }
}

impl Shared {
    /// What the worker does when its wait for an order ends: when it `took` one, the count
    /// goes up and the waiters for room are woken. Taking the lock between the count and the
    /// wake-up keeps a waiter that has just read the old count from missing the wake-up.
    pub(super) fn took(&self, took: bool) {
        if took {
            self.taken.fetch_add(1, Ordering::SeqCst);
            drop(self.lock());
            self.changed.notify_all();
        }
    }

    /// [`Worker::wait_room`].
    fn wait_room(&self, seen: u64, limit: Duration) -> bool {
        let waiting = |_: &mut super::Published| {
            self.taken.load(Ordering::SeqCst) == seen && !self.ended.load(Ordering::SeqCst)
        };
        let (published, waited) = self
            .changed
            .wait_timeout_while(self.lock(), limit, waiting)
            .unwrap_or_else(PoisonError::into_inner);
        drop(published);
        !waited.timed_out()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;

    use super::*;

    /// A hang guard, not a time any test expects to pass.
    const GUARD: Duration = Duration::from_secs(20);

    /// Asserts that only a wait that took an order counts: a wait that ended on the step
    /// interval leaves the count as it was. Catches a timeout counted as room, which would
    /// send a poller that holds messages back to a queue that is still full every interval.
    #[test]
    fn only_an_order_taken_counts_as_room() {
        let shared = Shared::default();
        shared.took(false);
        assert_eq!(shared.taken.load(Ordering::SeqCst), 0);
        shared.took(true);
        shared.took(true);
        assert_eq!(shared.taken.load(Ordering::SeqCst), 2);
    }

    /// Asserts that a wait for room ends at once when the worker took an order after the
    /// count the caller read (taken between the read and the refused delivery), and when the
    /// worker has ended; and that with neither it runs out its bound (here none) and says so.
    /// Catches a wait that misses room made before it began, and one that waits on a worker
    /// that will never take an order again.
    #[test]
    fn a_wait_for_room_ends_on_an_order_taken_since_the_count_or_the_end() {
        let shared = Shared::default();
        assert!(!shared.wait_room(0, Duration::ZERO), "no room yet");
        shared.took(true);
        assert!(shared.wait_room(0, GUARD), "taken after the count was read");
        assert!(!shared.wait_room(1, Duration::ZERO));
        shared.ended.store(true, Ordering::SeqCst);
        assert!(shared.wait_room(1, GUARD), "the worker ended");
    }

    /// Asserts that a caller waiting for room on another thread is woken by the order the
    /// worker takes. Catches a count raised without the wake-up, which would leave the
    /// messages held until the bound of the wait.
    #[test]
    fn an_order_taken_wakes_a_caller_waiting_for_room() {
        let shared = Arc::new(Shared::default());
        let waiter = {
            let shared = Arc::clone(&shared);
            thread::spawn(move || shared.wait_room(0, GUARD))
        };
        shared.took(true);
        assert!(waiter.join().unwrap(), "woken by the order, not the guard");
    }
}
