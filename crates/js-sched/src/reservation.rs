//! How each large request's reservation went, as the scheduler reports it to its
//! observer: one [`ReservationRecord`] per request when it leaves the queue (it starts, or
//! it is cancelled), and one [`LeaseRelease`] per lease when the lease ends.
//!
//! A large request is one that fits no worker now and that the scheduler holds a worker
//! for while that worker empties: a whole-machine request, an action that owns its worker,
//! or a blocked head of the queue. Its record says when it was first blocked, when it was
//! first reserved and the start the pick then expected, the last start expected, how many
//! times its hold moved to another worker, the core-seconds of work placed as backfill on
//! the worker it held, and when and how it left the queue.
//!
//! As a [`crate::Requeue`] is, a record changes no state outside the scheduler, so it is
//! not an [`js_types::sched::Effect`]. The scheduler keeps them only when asked
//! ([`crate::Scheduler::recording_reservations`]), for the caller to drain
//! ([`crate::Scheduler::take_reservations`]). Nothing the scheduler decides reads them, so
//! replaying the same inputs gives the same state either way. They live in memory only:
//! no log record, store row or wire message carries them.

use std::time::Duration;

use js_types::sched::{FarmTime, LeaseId, OperationId, WorkerId};

/// What the scheduler reports to its observer about reservations, in the order it
/// happened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReservationEvent {
    /// A large request left the queue: one per request and time it was blocked.
    Ended(ReservationRecord),
    /// A lease ended, whatever its request.
    Released(LeaseRelease),
}

/// How one large request's reservation went, from the round it was first blocked to the
/// one it left the queue in.
///
/// A request blocked again after it started (its lease given up and the request queued
/// again) is a new reservation, with a record of its own when it leaves the queue again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReservationRecord {
    /// The request.
    pub operation: OperationId,
    /// The first placement round it was blocked in: a whole-machine request or an action
    /// that owns its worker that was not placed (no worker fit it, or the round had placed
    /// its limit or a whole-node cap kept it off a quiet worker before it was reached), or
    /// the round it first held a worker as a blocked head.
    pub blocked_at: FarmTime,
    /// The first round it held a worker; `None` if it never held one.
    pub reserved_at: Option<FarmTime>,
    /// The start the pick expected on the worker it first held, in that round; `None`
    /// when it never held one or the pick did not know when the request could start
    /// (a hold made without whole-node settings, say).
    pub estimated_start: Option<FarmTime>,
    /// The start expected on the worker it held in the last round it held one; `None`
    /// as for `estimated_start`.
    pub last_estimate: Option<FarmTime>,
    /// How many times its hold went to another worker than the one it held before.
    pub moves: u32,
    /// The work placed as backfill on the worker it held while it waited, in
    /// core-seconds (booked cores times the time from each lease's grant to its end, or
    /// to this record if the lease still runs), rounded down.
    pub backfilled_core_seconds: u64,
    /// When it left the queue: for [`ReservationEnd::Started`], its actual start (the
    /// round its lease was granted in).
    pub ended_at: FarmTime,
    /// How it left the queue.
    pub end: ReservationEnd,
}

/// How a large request left the queue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReservationEnd {
    /// It was granted a lease on `worker`.
    Started {
        /// The worker it was granted.
        worker: WorkerId,
    },
    /// It was cancelled ([`crate::Scheduler::cancel_whole`]).
    Cancelled,
    /// It was refused, its refusal committed (no worker could run it for the unservable
    /// wait, or it lost too many leases).
    Refused,
    /// A result of an earlier run of it was committed while it waited to run again.
    Finished,
    /// Another scheduler of the same state store took it
    /// ([`crate::Scheduler::forget_queued`]).
    TakenElsewhere,
}

/// A lease that ended, with what placement expected of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeaseRelease {
    /// The lease.
    pub lease: LeaseId,
    /// Its request.
    pub operation: OperationId,
    /// The worker it held.
    pub worker: WorkerId,
    /// When it was granted.
    pub granted_at: FarmTime,
    /// When it ended.
    pub released_at: FarmTime,
    /// How long placement expected it to run: its backfill duration
    /// ([`crate::Backfill`]), the P90 of its key's wall minutes cut to its enforced max
    /// wall time, else that bound. `None` when backfill is off, its bound is not known or
    /// not trusted, or the lease was recovered rather than placed by this scheduler.
    pub estimate: Option<Duration>,
    /// Whether it was placed as backfill: on a worker held for a large request (or held
    /// from outside, [`crate::Scheduler::reserve_outside`]) because it was expected to end
    /// before that request could start there.
    pub backfilled: bool,
}
