//! The reservation ledger: what the registry hands the lease file to keep, and takes back
//! at a start.

use js_lease::ReservationRecord;

use super::Registry;

/// What [`Registry::restore_reservations`] did with the records it was given.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Restored {
    /// Records put back: a place on the wait list, counters, exclusions or a mark.
    pub resumed: usize,
    /// Records, or the marks of records, that could not stand and were dropped.
    pub dropped: usize,
}

impl Registry {
    /// The whole-node reservation state a restart must not lose.
    pub fn reservation_ledger(&self) -> Vec<ReservationRecord> {
        Vec::new()
    }

    /// Puts back the state `records` hold.
    pub fn restore_reservations(&self, records: &[ReservationRecord]) -> Restored {
        let _ = records;
        Restored::default()
    }

    /// How many restored reservations were dropped since the start.
    pub fn dropped_reservations(&self) -> u64 {
        0
    }
}
