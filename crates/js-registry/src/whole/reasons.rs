//! Why a whole-node lease failed to become ready, and what each reason does (design
//! `whole-node.md`, section 5, items 3 and 4).
//!
//! The reasons are data: every row is the pair (what happens to the host, what happens
//! to the request), so a new reason is a new row and no branch of the registry's logic.
//! The scheduler reads the code only. The detail the agent sent is kept for a person and
//! is never looked at here.

use js_agent_wire::RefusalReason;

/// What a failed prepare does to the host that failed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostEffect {
    /// The host is left out of this request's placements for `reserve_backoff`.
    ExcludeForBackoff,
    /// The host is left out of this request's placements for the rest of its life.
    ExcludeForLife,
    /// The host is tainted, as for any finding; a tainted host takes no lease.
    Taint,
}

/// What a failed prepare does to the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RequestEffect {
    /// The request counts one prepare failure and is placed again on another host.
    Requeue,
}

/// One row of the table of reasons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Row {
    pub(crate) host: HostEffect,
    pub(crate) request: RequestEffect,
}

const BACKOFF: Row = Row {
    host: HostEffect::ExcludeForBackoff,
    request: RequestEffect::Requeue,
};

/// The rows, in the order of [`RefusalReason`]'s variants (a unit test holds the order).
/// A code the wire does not know has already been read as `SetupFailed`.
const ROWS: [(RefusalReason, Row); 5] = [
    (RefusalReason::NotQuiet, BACKOFF),
    (RefusalReason::NoiseOverBound, BACKOFF),
    (RefusalReason::SetupFailed, BACKOFF),
    (
        RefusalReason::HostUnsafe,
        Row {
            host: HostEffect::Taint,
            request: RequestEffect::Requeue,
        },
    ),
    (
        RefusalReason::Unsupported,
        Row {
            host: HostEffect::ExcludeForLife,
            request: RequestEffect::Requeue,
        },
    ),
];

/// The code recorded for a lease that was not ready by its deadline.
pub(crate) const PREPARE_TIMEOUT_CODE: &str = "prepare-timeout";

/// How a prepare failed: the agent refused with a code, or the deadline passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Cause {
    Refused(RefusalReason),
    Timeout,
}

impl Cause {
    /// The row of the table for this cause. A timeout acts as `setup-failed`.
    pub(crate) fn row(self) -> Row {
        let reason = match self {
            Self::Refused(reason) => reason,
            Self::Timeout => RefusalReason::SetupFailed,
        };
        ROWS[reason as usize].1
    }

    /// The code text a terminal failure names.
    pub(crate) fn code(self) -> String {
        match self {
            Self::Refused(reason) => reason.into(),
            Self::Timeout => PREPARE_TIMEOUT_CODE.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The table is indexed by the reason's position, so a row out of order, or a
    /// variant without a row, fails here.
    #[test]
    fn rows_sit_at_their_reasons_position() {
        for (at, (reason, _)) in ROWS.iter().enumerate() {
            assert_eq!(*reason as usize, at);
        }
    }

    #[test]
    fn a_timeout_acts_as_setup_failed_and_names_its_own_code() {
        assert_eq!(
            Cause::Timeout.row(),
            Cause::Refused(RefusalReason::SetupFailed).row()
        );
        assert_eq!(Cause::Timeout.code(), "prepare-timeout");
        assert_eq!(
            Cause::Refused(RefusalReason::HostUnsafe).code(),
            "host-unsafe"
        );
    }
}
