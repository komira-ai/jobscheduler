use js_types::{HostId, RefusalCode};

/// The answer to a start on a [`HostBook`](crate::HostBook).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartAck {
    /// The lease holds this host.
    Started {
        /// The host that was booked.
        host: HostId,
    },
    /// The book will not take it, for this reason.
    Refused(RefusalCode),
}

/// What a read of one lease in a [`HostBook`](crate::HostBook) found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Observed {
    /// The book holds the lease, on this host.
    Present {
        /// The host the lease holds.
        host: HostId,
    },
    /// The book holds no such lease.
    Absent,
    /// No answer.
    Unknown,
}

impl Observed {
    /// Whether a booking may be freed: only after an explicit
    /// [`Observed::Absent`]. [`Observed::Unknown`] never frees it.
    pub fn frees_booking(&self) -> bool {
        matches!(self, Self::Absent)
    }
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
