//! What a failed prepare of a whole-host job does to the host: the exclusion it earns
//! ([`Exclusion`], [`prepare_exclusion`]) and the taint a `host-unsafe` refusal puts on it
//! ([`prepare_taint`]). The reasons are the rows of [`super::reasons`].

use js_agent_wire::{RefusalReason, Taint};
use js_types::Timestamp;

use super::reasons::{Cause, HostEffect};

/// Until when a host is left out of a request's placements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Exclusion {
    /// Left out while the clock reads before this instant.
    Until(Timestamp),
    /// Left out for the rest of the request's life.
    ForLife,
}

/// How long the row of `cause` leaves the failing host out of the request's placements, a
/// back-off ending at `backoff_end` or the request's life; `None` for a row that taints the
/// host instead.
fn exclusion(cause: Cause, backoff_end: Timestamp) -> Option<Exclusion> {
    match cause.row().host {
        HostEffect::ExcludeForBackoff => Some(Exclusion::Until(backoff_end)),
        HostEffect::ExcludeForLife => Some(Exclusion::ForLife),
        HostEffect::Taint => None,
    }
}

/// The exclusion the table of reasons gives the host of a failed prepare, for the placement
/// engine to apply to a whole-host job: `refused` is the code the agent refused the prepare
/// with, or `None` when no ready report came by the deadline (which acts as `setup-failed`).
/// The answer is a back-off ending at `backoff_end` ([`Exclusion::Until`]) for a reason that
/// leaves the host out for a while, [`Exclusion::ForLife`] for one that leaves it out for
/// good, and `None` for one that taints the host (`host-unsafe`), which
/// [`crate::Registry::taint_after_prepare`] records.
#[must_use]
pub fn prepare_exclusion(
    refused: Option<RefusalReason>,
    backoff_end: Timestamp,
) -> Option<Exclusion> {
    exclusion(refused.map_or(Cause::Timeout, Cause::Refused), backoff_end)
}

/// The taint a refused `host-unsafe` prepare puts on its host.
pub(crate) fn prepare_taint() -> Taint {
    Taint {
        reason: String::from(RefusalReason::HostUnsafe),
        run_id: None,
        check: None,
    }
}
