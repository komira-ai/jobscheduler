//! A committed whole-node lease that ended as a failed prepare: what that changes besides
//! the host's own record.

use std::time::Duration;

use js_agent_wire::{RefusalReason, Taint};
use js_lease::Timestamp;
use js_provider::HostBook;
use js_types::{HostId, LeaseId};

use crate::runs::Runs;

use super::exclusive::Exclusive;
use super::reasons::{Cause, HostEffect};
use super::requests::{FailedPrepare, Failure, NotReady, Requests, Rules, exclusion};
use crate::PrepareFailure;

/// The parts of the state a failed prepare changes besides the host's own record.
pub(crate) struct Ending<'a> {
    pub(crate) book: &'a mut HostBook,
    pub(crate) runs: &'a mut Runs,
    /// The registry's request records: `None` under a placer that keeps them
    /// ([`crate::Placer::keeps_wait_list`]), which counts every failure itself.
    pub(crate) requests: Option<&'a mut Requests>,
    /// The failed prepares not yet handed to the allocation log.
    pub(crate) failed: &'a mut Vec<FailedPrepare>,
    pub(crate) rules: Rules,
    pub(crate) now: Timestamp,
    /// The placer, told of the lease's end as of any stop ([`crate::Placer::ended`]).
    pub(crate) placer: Option<&'a dyn crate::Placer>,
}

impl Ending<'_> {
    /// Ends `lease`, committed to `host` and not ready, as a failed prepare: the lease
    /// reads `Absent` from now on, its runs are asked to stop, the host restores, the
    /// host takes the row's effect and the request counts one failure: in the placer when
    /// it keeps the lease's request ([`crate::Placer::prepare_failed`]), else here.
    /// `detail` is kept as received and read by nothing here. `taint` and `exclusive` are
    /// the host's record.
    /// The failure is noted (`failed`) for the record of the booking's end.
    pub(crate) fn fail(
        &mut self,
        (taint, exclusive): (&mut Option<Taint>, &mut Exclusive),
        host: &HostId,
        lease: LeaseId,
        (cause, detail): (Cause, &str),
    ) {
        let row = cause.row();
        if row.host == HostEffect::Taint {
            taint.get_or_insert_with(prepare_taint);
        }
        let backoff = Duration::from_millis(self.rules.reserve_backoff_ms);
        let failure = Failure {
            host,
            now: self.now,
            backoff_end: self.now.plus(backoff),
            max_replace: self.rules.max_replace,
        };
        // A placer that keeps the whole-node queue keeps the request's count and bars: the
        // registry only lets go of the lease as its request's.
        let handed = PrepareFailure {
            exclusion: exclusion(cause, failure.backoff_end),
            not_ready: NotReady {
                code: cause.code(),
                detail: detail.to_string(),
            },
        };
        let kept = self
            .placer
            .is_some_and(|placer| placer.prepare_failed(&lease, &handed));
        if let Some(requests) = self.requests.as_deref_mut() {
            match kept {
                true => requests.release(&lease),
                false => requests.fail(&lease, cause, detail, failure),
            }
        }
        self.failed.push(FailedPrepare {
            lease,
            host: host.clone(),
            at: self.now,
            profile: self.runs.lease_profile(&lease),
        });
        self.runs.stop_lease(&lease);
        *exclusive = exclusive.stopped();
        self.book.stop(&lease);
        if let Some(placer) = self.placer {
            placer.ended(lease);
        }
    }
}

/// The taint a failed prepare records on its host when its reason taints the host
/// (`host-unsafe`), and the host has no taint yet.
pub(crate) fn prepare_taint() -> Taint {
    Taint {
        reason: String::from(RefusalReason::HostUnsafe),
        run_id: None,
        check: None,
    }
}
