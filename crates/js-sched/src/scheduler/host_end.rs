//! The time a worker's host is taken back ([`crate::Event::HostEnd`]) and what it does to
//! placement: work is placed there only if it ends by then, and its max wall time is cut to
//! the time left.

use std::time::Duration;

use js_types::sched::{FarmTime, OperationId, WorkerId};

use super::{Scheduler, WholeMark, Worker};

impl Worker {
    /// Whether work with max wall time `wall` placed at `now` ends by the time the host
    /// ends, exactly at it included. A worker with no end takes any work. Work with no
    /// max wall time does not end by any time, so a worker with an end takes it too: its
    /// wall time is then the time left ([`Scheduler::cap_wall`]).
    pub(crate) fn fits_before_end(&self, now: FarmTime, wall: Option<Duration>) -> bool {
        match (self.ends_at, wall) {
            (Some(end), Some(wall)) => now.saturating_add(wall) <= end,
            _ => true,
        }
    }
}

impl Scheduler {
    /// When the host of `worker` ends ([`crate::Event::HostEnd`]); `None` when it does not,
    /// or the worker is not registered.
    #[must_use]
    pub fn host_end(&self, worker: &WorkerId) -> Option<FarmTime> {
        self.workers.get(worker).and_then(|w| w.ends_at)
    }

    /// The longest `operation` may run once leased: the time given with
    /// [`Scheduler::set_max_wall`], cut to the time its worker's host has left when it was
    /// placed on one that ends. The caller reads it when it builds the lease. `None` for an
    /// operation with no max wall time that was not placed on a worker with an end.
    #[must_use]
    pub fn max_wall(&self, operation: OperationId) -> Option<Duration> {
        self.max_walls.get(&operation).copied()
    }

    /// Cuts the max wall time of `operation`, just placed on `worker`, to the time left
    /// until the host of `worker` ends; an operation with none gets that time. Nothing
    /// changes on a worker with no end.
    pub(super) fn cap_wall(&mut self, operation: OperationId, worker: &WorkerId) {
        let Some(end) = self.host_end(worker) else {
            return;
        };
        let left = end.saturating_duration_since(self.now);
        let wall = self.max_wall(operation).map_or(left, |wall| wall.min(left));
        self.max_walls.insert(operation, wall);
    }

    /// The host end of `worker` as farm milliseconds, the latest time there is for a worker
    /// with none.
    pub(super) fn host_end_ms(&self, worker: &WorkerId) -> u64 {
        self.host_end(worker)
            .map_or(u64::MAX, |end| end.as_millis())
    }

    /// The host end ([`Scheduler::host_end_ms`]) of the worker `operation` holds, the
    /// latest time there is for an operation that holds none.
    pub(super) fn lease_host_end(&self, operation: OperationId) -> u64 {
        self.ops[&operation]
            .state
            .holding()
            .map_or(u64::MAX, |(_, worker)| self.host_end_ms(worker))
    }

    /// `mark` with its deadline no later than the host end of its worker: the hold is
    /// given up by the time the worker is gone.
    pub(super) fn end_bounded(&self, mut mark: WholeMark) -> WholeMark {
        let end = self.host_end_ms(&mark.worker);
        mark.deadline = FarmTime::from_millis(mark.deadline.as_millis().min(end));
        mark
    }
}
