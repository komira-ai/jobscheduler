//! The worker's part in a handover (see [`crate::lifecycle::handover`]): once the worker is
//! closed and its lifecycle is quiet for a handover, it stops for good at that point, so that
//! nothing it does after the session looked (a cleanup it begins for a job that ended, whose
//! checks would then be owed) is cut by the end of the process.
//!
//! The look and the stop are made on the worker's thread, between two orders, so no step can
//! come between them. A parked worker takes no order and makes no step again: it answers a
//! settle and another park, drops every other order, and ends when its queue is gone.

use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::time::Duration;

use super::{Order, Shared, Worker};
use crate::lifecycle::Lifecycle;

/// What became of a park the session asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Parked {
    /// The worker stopped for good with the lifecycle quiet: the process may end.
    Parked,
    /// A start, a stop or a finished job's cleanup is under way, the worker is not closed, or
    /// it did not answer within the wait (it is inside a long order or step, and takes the
    /// park when it is out of it): it goes on, and the session asks again later.
    Busy,
    /// The worker's thread is gone: nothing it holds can change any more.
    Ended,
}

impl Worker {
    /// Asks the worker to park (see the module): it answers once every order sent before
    /// this one is done, and this waits for the answer at most `wait`, so a worker stuck in a
    /// slow call never holds the session (its heartbeats, the bound of the handover, a second
    /// stop request). Only a closed worker ([`Worker::close`]) parks; one that takes the order
    /// after the wait parks then, and answers the next ask at once.
    pub fn park(&self, wait: Duration) -> Parked {
        let (done, answer) = sync_channel(1);
        // A worker that is behind is asked again later. One that is gone, or ends before it
        // takes the order, drops the order and with it `done`, which the wait below reads.
        if let Err(TrySendError::Full(_)) = self.handle.orders.try_send(Order::Park(done)) {
            return Parked::Busy;
        }
        match answer.recv_timeout(wait) {
            Ok(true) => Parked::Parked,
            Ok(false) | Err(RecvTimeoutError::Timeout) => Parked::Busy,
            Err(RecvTimeoutError::Disconnected) => Parked::Ended,
        }
    }
}

/// Answers a park: parks for good when the worker is closed and `life` is quiet for a
/// handover, and returns once the queue is gone; `false` (answered as busy) otherwise.
pub(super) fn park(
    life: &Lifecycle,
    shared: &Shared,
    done: &SyncSender<bool>,
    inbox: &Receiver<Order>,
) -> bool {
    let ready = shared.closed.load(Ordering::SeqCst) && life.quiet_for_handover();
    let _ = done.send(ready);
    if ready {
        // A parked worker is never inside an order or a step: it is not stalled.
        shared.busy_until.store(0, Ordering::SeqCst);
        for order in inbox {
            match order {
                Order::Settle(done) => {
                    let _ = done.send(());
                }
                Order::Park(done) => {
                    let _ = done.send(true);
                }
                _ => {}
            }
        }
    }
    ready
}
