use js_types::{HostId, LeaseId, MemoryHeadroom, Shape};

use crate::RefusalCode;

/// The provider calls that can answer Unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Call {
    /// [`crate::Provider::start`].
    Start,
    /// [`crate::Provider::stop`].
    Stop,
    /// [`crate::Provider::observe`].
    Observe,
}

/// The answer to a start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartAck {
    /// The lease holds this host.
    Started {
        /// The host that was booked.
        host: HostId,
    },
    /// The provider will not run it, for this reason.
    Refused(RefusalCode),
    /// No answer, or a partial one. The start may or may not have taken effect;
    /// look the lease up with `observe` before any retry.
    Unknown,
}

/// What a read of one lease found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Observed {
    /// The provider confirms the lease, on this host.
    Present {
        /// The host the lease holds.
        host: HostId,
    },
    /// The provider answered that the lease does not exist.
    Absent,
    /// No answer, a partial answer, or an error other than not-found.
    Unknown,
}

impl Observed {
    /// Whether the scheduler may free the booking: only after an explicit
    /// [`Observed::Absent`]. [`Observed::Unknown`] never frees it.
    pub fn frees_booking(&self) -> bool {
        matches!(self, Self::Absent)
    }
}

/// The answer to a stop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopAck {
    /// The provider stopped the lease, or found it already gone, and read this back.
    Stopped {
        /// The state read after the stop.
        read_back: Observed,
    },
    /// No answer. The stop may or may not have taken effect.
    Unknown,
}

/// One host of a provider's capacity and who holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostCapacity {
    /// The host.
    pub host: HostId,
    /// What the host offers to be booked: its memory is already cut to the pool's
    /// `memory_schedulable_percent`.
    pub shape: Shape,
    /// The memory the operator configured for the host, the percent of it that is booked
    /// and what that comes to (the memory of `shape`).
    pub memory: MemoryHeadroom,
    /// The lease booked first on the host, or `None` when it is free. A host that books
    /// many leases has more: read [`HostCapacity::leases`] to see them all.
    pub leased_to: Option<LeaseId>,
    /// Every lease booked on the host, in the order they were booked. Empty when the host
    /// is free; `leased_to` is the first of them.
    pub leases: Vec<LeaseId>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts that only an explicit absence frees a booking and Unknown and Present
    /// do not.
    #[test]
    fn only_absent_frees_a_booking() {
        let host = HostId::new("h").unwrap();
        assert!(Observed::Absent.frees_booking());
        assert!(!Observed::Unknown.frees_booking());
        assert!(!Observed::Present { host }.frees_booking());
    }
}
