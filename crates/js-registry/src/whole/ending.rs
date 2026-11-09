//! A committed whole-node lease that ended as a failed prepare: what that changes besides
//! the host's own record.

use std::time::Duration;

use js_agent_wire::{RefusalReason, Taint};
use js_provider::HostBook;
use js_types::Timestamp;
use js_types::{HostId, LeaseId};

use crate::runs::Runs;

use super::exclusive::Exclusive;
use super::reasons::{Cause, HostEffect};
use super::requests::{FailedPrepare, NotReady, Rules, exclusion};
use crate::PrepareFailure;

/// The parts of the state a failed prepare changes besides the host's own record.
pub(crate) struct Ending<'a> {
    pub(crate) book: &'a mut HostBook,
    pub(crate) runs: &'a mut Runs,
    /// The failed prepares not yet handed to the allocation log.
    pub(crate) failed: &'a mut Vec<FailedPrepare>,
    pub(crate) rules: Rules,
    pub(crate) now: Timestamp,
    /// The placer, which counts the failure toward the request's `max_replace` and keeps
    /// the host from it ([`crate::Placer::prepare_failed`]), and is told of the lease's end
    /// as of any stop ([`crate::Placer::ended`]).
    pub(crate) placer: &'a dyn crate::Placer,
}

impl Ending<'_> {
    /// Ends `lease`, committed to `host` and not ready, as a failed prepare: the lease
    /// reads `Absent` from now on, its runs are asked to stop, the host restores, the
    /// host takes the row's effect and the request counts one failure in the placer
    /// ([`crate::Placer::prepare_failed`]).
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
        // The placer keeps the request's count and bars.
        let handed = PrepareFailure {
            exclusion: exclusion(cause, self.now.plus(backoff)),
            not_ready: NotReady {
                code: cause.code(),
                detail: detail.to_string(),
            },
        };
        let _ = self.placer.prepare_failed(&lease, &handed);
        self.failed.push(FailedPrepare {
            lease,
            host: host.clone(),
            at: self.now,
            profile: self.runs.lease_profile(&lease),
        });
        self.runs.stop_lease(&lease);
        *exclusive = exclusive.stopped();
        self.book.stop(&lease);
        self.placer.ended(lease);
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
