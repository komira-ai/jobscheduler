//! Where a registered host stands in an exclusive (whole-node) lease: committed to a
//! lease and preparing, ready for it, or restoring after it. In memory only.
//!
//! The states are transitions of one value, so the registry cannot reach a state by
//! assigning a field and the rules read in one place:
//!
//! - **Committed.** The commit gate (a quiet, bookable host) passed and the lease holds
//!   the host. The host's replies say `exclusive`, the lease reads `Unknown` (the
//!   consumer's lease stays pending), and no run is queued or started.
//! - **Ready.** The agent reported `exclusive_ready` for exactly this lease. The lease
//!   reads `Present`, and the lease holder's runs are queued and started as for any lease.
//! - **Restoring.** The lease ended. The host is bookable again only after a heartbeat
//!   sent after the agent was answered without `exclusive`: the heartbeat that crossed
//!   the stop on the wire says nothing about the restore, so the first heartbeat after
//!   the stop only marks the agent as told, and the second ends the state. Whether the
//!   host then books is the ordinary rule, read from the last heartbeat at the start:
//!   phase `serving`, no held run, no live run.
//!
//! A whole-node booking restored by a restart (`seat`) starts `Committed`, never `Free`:
//! the preparation was in memory only, so the agent reports ready for it again. The one
//! exception is a host that already lists held runs: a whole-node host is quiet when the
//! lease commits and takes no run before the lease is ready, so a run on it can only
//! belong to the lease, which was therefore ready before the restart. That lease is
//! seated `Ready`, so its consumer is not refused `not-ready` until those runs end.
//!
//! A ready report is taken only as the transition `Committed` to `Ready`, for the lease
//! the host is committed to and a clean host. A report for any other lease, for a host in
//! another state (free, ready, restoring), or from a tainted host changes nothing and the
//! heartbeat is answered as usual: the report is a claim of the agent, and a claim that
//! does not match what the registry holds is not an error.
//!
//! A committed lease that is not reported ready within `prepare_timeout` of its commit
//! is overdue ([`Exclusive::overdue`]); the registry then ends it as a failed prepare,
//! excluding or tainting the host as the cause says. Only `Committed` is overdue: a
//! lease seated `Ready` (a restored booking with live runs, see [`Exclusive::seated`])
//! has been prepared and never times out.
//!
//! A host that says hello again lost its session, so its preparation is void: `Ready`
//! falls back to `Committed` (the same lease and commit time) until it reports again.

use js_lease::Timestamp;
use js_types::LeaseId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Exclusive {
    /// No exclusive lease is on the host and none is being undone.
    Free,
    /// `lease` holds the host since `at`, and the agent has not reported ready.
    Committed { lease: LeaseId, at: Timestamp },
    /// The agent reported ready for `lease`, committed at `at`.
    Ready { lease: LeaseId, at: Timestamp },
    /// The lease ended. `told` is whether a heartbeat reply without `exclusive` has gone
    /// to the agent since.
    Restoring { told: bool },
}

impl Exclusive {
    /// Whether the host may be booked as far as this state goes: nothing is held on it
    /// or being undone.
    pub(crate) fn is_free(&self) -> bool {
        *self == Self::Free
    }

    /// The lease id the heartbeat reply names, while a lease holds the host.
    pub(crate) fn granted(&self) -> Option<String> {
        match self {
            Self::Committed { lease, .. } | Self::Ready { lease, .. } => Some(lease.to_string()),
            Self::Free | Self::Restoring { .. } => None,
        }
    }

    /// Whether the lease holding the host is committed and the agent has not reported
    /// it ready. The caller has found the lease on the host, so the state names it: no
    /// lease id is compared here.
    pub(crate) fn awaits_ready(&self) -> bool {
        matches!(self, Self::Committed { .. })
    }

    /// The lease committed to the host and not yet reported ready.
    pub(crate) fn committed(&self) -> Option<LeaseId> {
        match self {
            Self::Committed { lease, .. } => Some(*lease),
            Self::Free | Self::Ready { .. } | Self::Restoring { .. } => None,
        }
    }

    /// The lease the agent reported ready.
    pub(crate) fn ready_lease(&self) -> Option<LeaseId> {
        match self {
            Self::Ready { lease, .. } => Some(*lease),
            Self::Free | Self::Committed { .. } | Self::Restoring { .. } => None,
        }
    }

    /// The committed lease that has waited `timeout_ms` or more since its commit: the
    /// deadline is the same for a slow agent, a silent one and a host that went lost.
    pub(crate) fn overdue(&self, now: Timestamp, timeout_ms: u64) -> Option<LeaseId> {
        let waited = |at: &Timestamp| now.millis().saturating_sub(at.millis());
        match self {
            Self::Committed { lease, at } if waited(at) >= timeout_ms => Some(*lease),
            _ => None,
        }
    }

    /// The state after the agent reported `reported` ready, on a host that is `clean`
    /// (not tainted). Only the committed lease, named exactly, becomes ready.
    pub(crate) fn report_ready(self, reported: &str, clean: bool) -> Self {
        match self {
            Self::Committed { lease, at }
                if [clean, lease.to_string() == reported]
                    .into_iter()
                    .all(|ok| ok) =>
            {
                Self::Ready { lease, at }
            }
            other => other,
        }
    }

    /// The state of a whole-node booking put back after a restart, `at` the restore:
    /// `Ready` when the host lists held runs (see the module documentation), else
    /// `Committed`.
    pub(crate) fn seated(lease: LeaseId, at: Timestamp, runs_held: bool) -> Self {
        match runs_held {
            true => Self::Ready { lease, at },
            false => Self::Committed { lease, at },
        }
    }

    /// The state after the lease holding the host ended (the caller found it there).
    pub(crate) fn stopped(self) -> Self {
        match self {
            Self::Committed { .. } | Self::Ready { .. } => Self::Restoring { told: false },
            other => other,
        }
    }

    /// The state after a heartbeat: the first after the stop marks the agent as told, the
    /// next ends the restore.
    pub(crate) fn after_beat(self) -> Self {
        match self {
            Self::Restoring { told: false } => Self::Restoring { told: true },
            Self::Restoring { told: true } => Self::Free,
            other => other,
        }
    }

    /// The state after the host said hello again.
    pub(crate) fn after_hello(self) -> Self {
        match self {
            Self::Ready { lease, at } => Self::Committed { lease, at },
            Self::Restoring { .. } => Self::Restoring { told: false },
            other => other,
        }
    }
}
