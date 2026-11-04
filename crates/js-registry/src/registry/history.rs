//! The record of a lease booking that ended ([`js_usage::EndedRow`]): what the registry
//! knows of it when the lease is stopped, and the log it hands the record to.
//!
//! The record is read in [`Registry::stop`] before the stop changes anything, and handed to
//! the log after the registry's lock is released: a log never runs under it. A lease booked
//! on a host gives one record, at the stop that frees the booking; a stop of a lease whose
//! whole-node request still waited for a host gives one with no host; a later stop of the
//! same lease finds neither and gives none.
//!
//! What the registry can tell of why a lease ended: its host was lost (`lost`), or it ended
//! at or after the latest end its wall limit allows (`stopped-at-wall-limit`), or its request
//! was cancelled while it waited (`cancelled-while-waiting`); anything else is `released`. A
//! stop does not say whether the lease was released or ran out of time, and a whole-node
//! prepare that failed frees its booking outside a stop, so neither `expired` nor
//! `failed-to-start` is written here.

use std::sync::Arc;

use js_lease::Timestamp;
use js_provider::Observed;
use js_types::{Admission, HostId, LeaseId};
use js_usage::{AllocationId, AllocationLog, Booked, DrainOutcome, EndedReason, EndedRow};

use super::record::Queue;
use super::{HostState, Registry, State};
use crate::lives;
use crate::whole::requests::RequestKey;

/// A record and the log it goes to.
pub(super) type Ended = Option<(Arc<dyn AllocationLog>, EndedRow)>;

/// The end at `actual` less the estimate, clamped to the range of an `i64`.
fn error_ms(actual: u64, estimate: u64) -> i64 {
    let error = i128::from(actual) - i128::from(estimate);
    let error = error.clamp(i128::from(i64::MIN), i128::from(i64::MAX));
    // The clamp keeps it in range.
    i64::try_from(error).unwrap_or_default()
}

impl Registry {
    /// Gives the registry the log that takes the record of every lease booking that ends
    /// ([`js_usage::AllocationLog`]): from then on each stop that frees a booking, or
    /// cancels a whole-node request still waiting for a host, hands it one record, after the
    /// registry's lock is released. A registry with no log records nothing. The log is set
    /// once (a `OnceLock`): only the first call takes effect.
    pub fn set_allocation_log(&self, log: Arc<dyn AllocationLog>) {
        let _ = self.shared.log.set(log);
    }

    /// Whether the pool's class books a whole node for a request of its own.
    pub(super) fn whole_admission(&self) -> bool {
        matches!(self.shared.config.class().admission(), Admission::Whole)
    }

    /// The record of `lease` ending at `ended_at` (ms on the registry's clock) as `state`
    /// stands before the stop, with the log it goes to; `None` without a log, or for a lease
    /// neither booked nor waiting.
    pub(super) fn ended_row(&self, state: &State, lease: &LeaseId, ended_at: u64) -> Ended {
        let log = self.shared.log.get()?.clone();
        let row = match state.book.read(lease) {
            Observed::Present { host } => self.booked_row(state, lease, host, ended_at),
            Observed::Absent | Observed::Unknown => waiting_row(state, lease, ended_at)?,
        };
        Some((log, row))
    }

    /// Hands the record to its log. Call it with the registry's lock released.
    pub(super) fn log_ended(&self, ended: Ended) {
        ended.into_iter().for_each(|(log, row)| log.ended(row));
    }

    /// The record of `lease`, booked on `host`.
    fn booked_row(&self, state: &State, lease: &LeaseId, host: HostId, ended_at: u64) -> EndedRow {
        let record = state.records.get(&host);
        let bounds = record.into_iter().flat_map(|record| record.bounds.iter());
        let bound = bounds.copied().find(|bound| bound.lease == *lease);
        let profiles = state.runs.profiles_of(lease);
        let source = self.shared.lives.get();
        let estimate = bound
            .zip(source)
            .and_then(|(bound, lives)| lives::estimate_end(bound.started_at, &profiles, &**lives));
        let now = Timestamp::from_millis(ended_at);
        let grace = self.grace_ms();
        let lost = record.is_some_and(|record| record.state(now, grace) == HostState::Lost);
        let at_limit = bound.is_some_and(|bound| ended_at >= bound.end.millis());
        let ended_reason = match (lost, at_limit) {
            (true, _) => EndedReason::Lost,
            (false, true) => EndedReason::StoppedAtWallLimit,
            (false, false) => EndedReason::Released,
        };
        // A lease put back after a restart without a source of lives has no bound.
        let queue = bound.map_or(Queue::restored(self.whole_admission()), |bound| bound.queue);
        let booked = state.book.held(lease).map(|held| Booked {
            whole_host: held.whole,
            cpu_milli: held.resources.cpu_millis(),
            memory_bytes: held.resources.memory_bytes(),
        });
        EndedRow {
            id: AllocationId::of_lease(lease),
            profile: profiles.into_iter().next(),
            host: Some(host),
            booked,
            queued_ms: queue.queued_ms,
            run_ms: bound.map(|bound| ended_at.saturating_sub(bound.started_at)),
            ended_reason,
            drain_outcome: queue.drain,
            give_up_outcome: queue.give_ups,
            estimated_end: estimate,
            actual_end: ended_at,
            error_ms: estimate.map(|estimate| error_ms(ended_at, estimate)),
            used: None,
        }
    }
}

/// The record of `lease` whose whole-node request waits for a host and is cancelled by the
/// stop at `ended_at`; `None` when no request of the lease waits.
fn waiting_row(state: &State, lease: &LeaseId, ended_at: u64) -> Option<EndedRow> {
    let (id, since) = state.waiting.presented_by(lease)?;
    let drain = match state.marks.host_of(id).is_some() {
        true => DrainOutcome::MarkDropped,
        false => DrainOutcome::NoMark,
    };
    let give_ups = state.requests.give_ups(&RequestKey::from(id));
    Some(EndedRow {
        id: AllocationId::of_lease(lease),
        profile: state.runs.profiles_of(lease).into_iter().next(),
        host: None,
        booked: None,
        queued_ms: Some(ended_at.saturating_sub(since.millis())),
        run_ms: Some(0),
        ended_reason: EndedReason::CancelledWhileWaiting,
        drain_outcome: drain,
        give_up_outcome: Some(give_ups),
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
    /// dropped (or the reverse), a request with give-ups and no mark of its own reported as
    /// holding none, and a lease put back after a restart given a whole-node request's queue
    /// it never kept.
    #[test]
    fn a_queue_is_the_wait_the_mark_and_the_give_ups_of_the_request() {
        let now = Timestamp::from_millis(50_000);
        let since = Some(Timestamp::from_millis(10_000));
        let held = Queue::whole(now, since, true, 2);
        let expected = Queue {
            queued_ms: Some(40_000),
            drain: DrainOutcome::MarkHeld,
            give_ups: Some(2),
        };
        assert_eq!(held, expected);
        let drain = |held, give_ups| Queue::whole(now, since, held, give_ups).drain;
        assert_eq!(drain(false, 1), DrainOutcome::MarkDropped);
        assert_eq!(drain(false, 0), DrainOutcome::NoMark);
        assert_eq!(Queue::whole(now, None, false, 0).queued_ms, Some(0));
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
