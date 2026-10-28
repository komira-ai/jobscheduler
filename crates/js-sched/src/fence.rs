//! Lease timing: the re-dispatch grace G on the scheduler's side and the self-fence T
//! on the worker's side.
//!
//! The constants and the argument that relates them live in [`js_types::sched::fence`],
//! so the scheduler and the host agent share one definition; they are re-exported here.
//! What stays in this module is [`SelfFence`], the worker's clock.

use js_types::sched::FarmTime;

pub use js_types::sched::fence::{
    HANDOVER_GRACE, LEADER_LEASE_MARGIN, LEASE_GRACE, SELF_FENCE, SESSION_SWITCH_GRACE,
    START_GRACE, START_VALIDITY,
};

/// A worker's self-fence clock, kept by the daemon.
///
/// The worker records the send time of each heartbeat the scheduler acknowledges. It
/// may start or keep running a self-fenced lease only while the newest of those is
/// less than [`SELF_FENCE`] old. A worker that has never been acknowledged may not run
/// one at all. Lease kinds with [`FencePolicy::RunOn`] ignore the fence.
///
/// [`FencePolicy::RunOn`]: js_types::sched::FencePolicy::RunOn
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SelfFence {
    newest_acked_send: Option<FarmTime>,
}

impl SelfFence {
    /// A fence that has seen no acknowledgement: it allows nothing.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            newest_acked_send: None,
        }
    }

    /// The scheduler acknowledged the heartbeat this worker sent at `sent_at`. An older
    /// acknowledgement arriving late never moves the deadline back.
    pub fn acknowledged(&mut self, sent_at: FarmTime) {
        self.newest_acked_send = Some(self.newest_acked_send.map_or(sent_at, |t| t.max(sent_at)));
    }

    /// The time at which self-fenced leases must stop, if any heartbeat was acknowledged.
    #[must_use]
    pub fn deadline(&self) -> Option<FarmTime> {
        self.newest_acked_send.map(|t| t.saturating_add(SELF_FENCE))
    }

    /// Whether a self-fenced lease may run at `now`.
    #[must_use]
    pub fn allows(&self, now: FarmTime) -> bool {
        self.deadline().is_some_and(|d| now < d)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: u64) -> FarmTime {
        FarmTime::from_millis(secs * 1_000)
    }

    /// Catches: a fence that allows work before any acknowledgement, measures T from
    /// the acknowledgement's arrival instead of the heartbeat's send time, or lets a
    /// late, older acknowledgement shorten the deadline.
    #[test]
    fn fence_runs_t_from_newest_acked_send() {
        let mut fence = SelfFence::new();
        assert!(!fence.allows(at(0)));
        fence.acknowledged(at(10));
        assert_eq!(fence.deadline(), Some(at(50)));
        assert!(fence.allows(at(49)));
        assert!(!fence.allows(at(50)));
        fence.acknowledged(at(20));
        fence.acknowledged(at(15));
        assert_eq!(fence.deadline(), Some(at(60)));
    }
}
