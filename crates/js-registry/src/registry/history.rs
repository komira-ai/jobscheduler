//! The record of a lease booking that ended ([`js_usage::EndedRow`]): what the registry
//! knows of it when the booking ends, and the log it hands the record to.
//!
//! A booking ends in one of two ways, and each gives one record:
//!
//! * [`Registry::stop`] of a lease booked on a host. The record is read before the stop
//!   changes anything. A stop of a lease whose whole-node request still waits for a host
//!   gives a record with no host; a later stop of the same lease finds neither and gives
//!   none.
//! * A whole-node prepare that failed (refused by the agent, or not ready within
//!   `prepare_timeout`) frees its booking under the registry's lock, in a heartbeat or an
//!   advance; the failure is noted then, and the record is read before the lock is
//!   released ([`Registry::release`]).
//!
//! Either way the record goes to the log after the registry's lock is released: a log never
//! runs under it.
//!
//! What the registry can tell of why a booking ended: its prepare failed
//! (`failed-to-start`), its host was lost (`lost`), it ended at or after the latest end its
//! wall limit allows (`stopped-at-wall-limit`), or its request was cancelled while it waited
//! (`cancelled-while-waiting`); anything else is `released`. A stop does not say whether the
//! lease was released or ran out of time, so `expired` is not written here.
//!
//! The estimated end is read once for a stopped lease ([`Registry::estimate_of`]), and the
//! same value goes into the record and the tail counters. The profile is the first a run of
//! the lease was queued with, kept by the runs table until the lease stops, however soon the
//! run itself is dropped.

use std::sync::{Arc, MutexGuard};

use js_provider::Observed;
use js_types::Timestamp;
use js_types::{Admission, HostId, LeaseId};
use js_usage::{
    AllocationId, AllocationLog, Booked, DrainOutcome, EndedReason, EndedRow, ProfileKey,
};

use super::record::{Bound, Queue};
use super::{HostState, Registry, State};
use crate::lives;
use crate::whole::requests::FailedPrepare;

/// A record and the log it goes to.
pub(super) type Ended = Option<(Arc<dyn AllocationLog>, EndedRow)>;

/// The end at `actual` less the estimate, clamped to the range of an `i64`.
fn error_ms(actual: u64, estimate: u64) -> i64 {
    let error = i128::from(actual) - i128::from(estimate);
    let error = error.clamp(i128::from(i64::MIN), i128::from(i64::MAX));
    // The clamp keeps it in range.
    i64::try_from(error).unwrap_or_default()
}

/// The host `lease` is booked on, if it is.
fn host_of(state: &State, lease: &LeaseId) -> Option<HostId> {
    match state.book.read(lease) {
        Observed::Present { host } => Some(host),
        Observed::Absent | Observed::Unknown => None,
    }
}

/// The bound `host` keeps for `lease`: its start, latest end and queue.
fn bound_of(state: &State, host: &HostId, lease: &LeaseId) -> Option<Bound> {
    let bounds = state.records.get(host).into_iter();
    let bounds = bounds.flat_map(|record| record.bounds.iter());
    bounds.copied().find(|bound| bound.lease == *lease)
}

/// How a booking ended: when, at what estimated end, why, what it booked and the profile
/// of its runs.
struct HowEnded {
    ended_at: u64,
    estimate: Option<u64>,
    reason: EndedReason,
    booked: Option<Booked>,
    profile: Option<ProfileKey>,
}

impl Registry {
    /// Gives the registry the log that takes the record of every lease booking that ends
    /// ([`js_usage::AllocationLog`]): from then on each stop that frees a booking or cancels
    /// a whole-node request still waiting for a host, and each failed prepare, hands it one
    /// record, after the registry's lock is released. A registry with no log records
    /// nothing. The log is set once (a `OnceLock`): only the first call takes effect.
    pub fn set_allocation_log(&self, log: Arc<dyn AllocationLog>) {
        let _ = self.shared.log.set(log);
    }

    /// Whether the pool's class books a whole node for a request of its own.
    pub(super) fn whole_admission(&self) -> bool {
        matches!(self.shared.config.class().admission(), Admission::Whole)
    }

    /// The end the estimate feed gives `lease` as `state` stands: its start plus the
    /// largest P90 among the profiles of its runs. `None` for a lease not booked, without a
    /// source of lives or a bound, or with no keyed run that has a P90.
    pub(super) fn estimate_of(&self, state: &State, lease: &LeaseId) -> Option<u64> {
        let host = host_of(state, lease)?;
        self.estimate_on(state, &host, lease)
    }

    fn estimate_on(&self, state: &State, host: &HostId, lease: &LeaseId) -> Option<u64> {
        let bound = bound_of(state, host, lease)?;
        let lives = self.shared.lives.get()?;
        let profiles = state.runs.profiles_of(lease);
        lives::estimate_end(bound.started_at, &profiles, &**lives)
    }

    /// The record of `lease` stopped at `ended_at` (ms on the registry's clock) as `state`
    /// stands before the stop, with `estimate` its estimated end, and the log it goes to;
    /// `None` without a log, or for a lease neither booked nor waiting.
    pub(super) fn ended_row(
        &self,
        state: &State,
        lease: &LeaseId,
        (ended_at, estimate): (u64, Option<u64>),
    ) -> Ended {
        let log = self.shared.log.get()?.clone();
        let row = match host_of(state, lease) {
            Some(host) => {
                let ending = HowEnded {
                    ended_at,
                    estimate,
                    reason: self.reason_of(state, &host, lease, ended_at),
                    booked: state.book.held(lease).map(|held| Booked {
                        whole_host: held.whole,
                        cpu_milli: held.resources.cpu_millis(),
                        memory_bytes: held.resources.memory_bytes(),
                    }),
                    profile: state.runs.lease_profile(lease),
                };
                self.hosted_row(state, lease, host, ending)
            }
            None => waiting_row(state, self.placer(), lease, ended_at)?,
        };
        Some((log, row))
    }

    /// Hands the record to its log. Call it with the registry's lock released.
    pub(super) fn log_ended(&self, ended: Ended) {
        ended.into_iter().for_each(|(log, row)| log.ended(row));
    }

    /// Releases `state`, handing the log the records of the prepares that failed while it was
    /// held. Every call that can fail a prepare (an advance, a heartbeat) releases its lock
    /// through here.
    pub(super) fn release(&self, mut state: MutexGuard<'_, State>) {
        let failed = std::mem::take(&mut state.failed);
        let rows: Vec<EndedRow> = failed
            .iter()
            .map(|failed| self.failed_row(&state, failed))
            .collect();
        drop(state);
        let deliver = self.shared.log.get().map(|log| (log, rows));
        deliver
            .into_iter()
            .for_each(|(log, rows)| rows.into_iter().for_each(|row| log.ended(row)));
    }

    /// The record of the booking a failed prepare freed: the whole host, ended when it failed.
    fn failed_row(&self, state: &State, failed: &FailedPrepare) -> EndedRow {
        let shape = self.shared.config.booked();
        let ending = HowEnded {
            ended_at: failed.at.millis(),
            estimate: self.estimate_on(state, &failed.host, &failed.lease),
            reason: EndedReason::FailedToStart,
            booked: Some(Booked {
                whole_host: true,
                cpu_milli: shape.resources().cpu_millis(),
                memory_bytes: shape.resources().memory_bytes(),
            }),
            profile: failed.profile.clone(),
        };
        self.hosted_row(state, &failed.lease, failed.host.clone(), ending)
    }

    /// Why `lease`, booked on `host`, ended at `ended_at` as far as a stop can tell.
    fn reason_of(
        &self,
        state: &State,
        host: &HostId,
        lease: &LeaseId,
        ended_at: u64,
    ) -> EndedReason {
        let now = Timestamp::from_millis(ended_at);
        let record = state.records.get(host);
        let lost =
            record.is_some_and(|record| record.state(now, self.grace_ms()) == HostState::Lost);
        let bound = bound_of(state, host, lease);
        let at_limit = bound.is_some_and(|bound| ended_at >= bound.end.millis());
        match (lost, at_limit) {
            (true, _) => EndedReason::Lost,
            (false, true) => EndedReason::StoppedAtWallLimit,
            (false, false) => EndedReason::Released,
        }
    }

    /// The record of `lease`, booked on `host`, that ended as `ending` says.
    fn hosted_row(
        &self,
        state: &State,
        lease: &LeaseId,
        host: HostId,
        ending: HowEnded,
    ) -> EndedRow {
        let bound = bound_of(state, &host, lease);
        // A lease put back after a restart without a source of lives has no bound.
        let queue = bound.map_or(Queue::restored(self.whole_admission()), |bound| bound.queue);
        let ended_at = ending.ended_at;
        EndedRow {
            id: AllocationId::of_lease(lease),
            profile: ending.profile,
            host: Some(host),
            booked: ending.booked,
            queued_ms: queue.queued_ms,
            run_ms: bound.map(|bound| ended_at.saturating_sub(bound.started_at)),
            ended_reason: ending.reason,
            drain_outcome: queue.drain,
            give_up_outcome: queue.give_ups,
            estimated_end: ending.estimate,
            actual_end: ended_at,
            error_ms: ending.estimate.map(|estimate| error_ms(ended_at, estimate)),
            used: None,
        }
    }
}

/// The record of `lease` whose whole-node request waits for a host and is cancelled by the
/// stop at `ended_at`, read from the placer's wait list; `None` when no request of the
/// lease waits. Its mark was dropped if it held one at any time while it
/// waited; it gave no host up.
fn waiting_row(
    state: &State,
    placer: &dyn crate::Placer,
    lease: &LeaseId,
    ended_at: u64,
) -> Option<EndedRow> {
    let waiter = placer.whole_waiter_of(lease)?;
    let since = waiter.since;
    let drain = match waiter.marked {
        true => DrainOutcome::MarkDropped,
        false => DrainOutcome::NoMark,
    };
    Some(EndedRow {
        id: AllocationId::of_lease(lease),
        profile: state.runs.lease_profile(lease),
        host: None,
        booked: None,
        queued_ms: Some(ended_at.saturating_sub(since.millis())),
        run_ms: Some(0),
        ended_reason: EndedReason::CancelledWhileWaiting,
        drain_outcome: drain,
        give_up_outcome: Some(0),
        estimated_end: None,
        actual_end: ended_at,
        error_ms: None,
        used: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: a whole-node request's wait counted from anything but its first
    /// presentation, a request that never waited given a wait, a mark that held reported as
    /// dropped (or the reverse), a give-up count other than zero, and a lease put back after
    /// a restart given a whole-node request's queue it never kept.
    #[test]
    fn a_queue_is_the_wait_and_the_mark_of_the_request_with_no_give_up() {
        let now = Timestamp::from_millis(50_000);
        let since = Some(Timestamp::from_millis(10_000));
        let held = Queue::whole(now, since, (true, true));
        let expected = Queue {
            queued_ms: Some(40_000),
            drain: DrainOutcome::MarkHeld,
            give_ups: Some(0),
        };
        assert_eq!(held, expected);
        let drain = |held, marked| Queue::whole(now, since, (held, marked)).drain;
        assert_eq!(drain(false, true), DrainOutcome::MarkDropped);
        assert_eq!(drain(false, false), DrainOutcome::NoMark);
        assert_eq!(Queue::whole(now, None, (false, false)).queued_ms, Some(0));
        assert_eq!(Queue::restored(false), Queue::ORDINARY);
        let restored = (None, DrainOutcome::Unknown, None);
        let whole = Queue::restored(true);
        assert_eq!((whole.queued_ms, whole.drain, whole.give_ups), restored);
    }

    /// Catches: an error with the wrong sign, and one past the range of an `i64` wrapping.
    #[test]
    fn the_error_is_the_actual_end_less_the_estimate_clamped() {
        assert_eq!(error_ms(130_000, 100_000), 30_000);
        assert_eq!(error_ms(90_000, 100_000), -10_000);
        assert_eq!(error_ms(u64::MAX, 0), i64::MAX);
        assert_eq!(error_ms(0, u64::MAX), i64::MIN);
    }
}
