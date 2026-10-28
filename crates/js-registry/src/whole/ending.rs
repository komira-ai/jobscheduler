//! A committed whole-node lease that ended as a failed prepare: what that changes besides
//! the host's own record.

use std::time::Duration;

use js_agent_wire::{RefusalReason, Taint};
use js_lease::Timestamp;
use js_pool_static::HostBook;
use js_types::{HostId, LeaseId};

use crate::runs::Runs;

use super::exclusive::Exclusive;
use super::reasons::{Cause, HostEffect};
use super::requests::{Failure, Requests, Rules};

/// The parts of the state a failed prepare changes besides the host's own record.
pub(crate) struct Ending<'a> {
    pub(crate) book: &'a mut HostBook,
    pub(crate) runs: &'a mut Runs,
    pub(crate) requests: &'a mut Requests,
    pub(crate) rules: Rules,
    pub(crate) now: Timestamp,
}

impl Ending<'_> {
    /// Ends `lease`, committed to `host` and not ready, as a failed prepare: the lease
    /// reads `Absent` from now on, its runs are asked to stop, the host restores, the
    /// host takes the row's effect and the request counts one failure. `detail` is kept as
    /// received and read by nothing here. `taint` and `exclusive` are the host's record.
    pub(crate) fn fail(
        &mut self,
        (taint, exclusive): (&mut Option<Taint>, &mut Exclusive),
        host: &HostId,
        lease: LeaseId,
        (cause, detail): (Cause, &str),
    ) {
        let row = cause.row();
        if row.host == HostEffect::Taint {
            let reason = String::from(RefusalReason::HostUnsafe);
            taint.get_or_insert(Taint {
                reason,
                run_id: None,
                check: None,
            });
        }
        let backoff = Duration::from_millis(self.rules.reserve_backoff_ms);
        let failure = Failure {
            host,
            now: self.now,
            backoff_end: self.now.plus(backoff),
            max_replace: self.rules.max_replace,
        };
        self.requests.fail(&lease, cause, detail, failure);
        self.runs.stop_lease(&lease);
        *exclusive = exclusive.stopped();
        self.book.stop(&lease);
    }
}
