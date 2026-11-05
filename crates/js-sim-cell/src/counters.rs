//! What an agent node counts: how a test, or a scenario's witness, shows that the
//! situation it is about happened.

use std::collections::BTreeMap;

/// One thing an agent node counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Counter {
    /// A start delivered for a run the agent process already holds or has run (and has
    /// not been told to stop).
    StartRepeated,
    /// A start delivered for a run the agent process was told to stop or cancel.
    StartAfterStop,
    /// A start the agent refused because it waited past its validity.
    StartDroppedLate,
    /// A result sent in a heartbeat after the first heartbeat that carried it.
    ResultResent,
    /// An answer the agent ignored, wholly or in part, because it failed to decode or
    /// validate, or named a start the cell cannot account for.
    ReplyInvalid,
}

impl Counter {
    /// Every counter, in order.
    pub const ALL: [Counter; 5] = [
        Counter::StartRepeated,
        Counter::StartAfterStop,
        Counter::StartDroppedLate,
        Counter::ResultResent,
        Counter::ReplyInvalid,
    ];

    /// A stable snake_case name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Counter::StartRepeated => "start_repeated",
            Counter::StartAfterStop => "start_after_stop",
            Counter::StartDroppedLate => "start_dropped_late",
            Counter::ResultResent => "result_resent",
            Counter::ReplyInvalid => "reply_invalid",
        }
    }
}

/// The counts of one agent node, over the life of its host: a replaced process does not
/// reset them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Counters(BTreeMap<Counter, u64>);

impl Counters {
    /// How many times `counter` was bumped.
    #[must_use]
    pub fn get(&self, counter: Counter) -> u64 {
        self.0.get(&counter).copied().unwrap_or(0)
    }

    /// Every counter bumped at least once, with its count, in [`Counter`] order.
    pub fn iter(&self) -> impl Iterator<Item = (Counter, u64)> + '_ {
        self.0.iter().map(|(counter, count)| (*counter, *count))
    }

    /// Adds one to `counter`.
    pub(crate) fn bump(&mut self, counter: Counter) {
        *self.0.entry(counter).or_insert(0) += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: a counter renamed in what a witness reads, or two counters that share a
    /// name.
    #[test]
    fn every_counter_has_its_own_stable_name() {
        let names = Counter::ALL.map(Counter::name);
        assert_eq!(
            names,
            [
                "start_repeated",
                "start_after_stop",
                "start_dropped_late",
                "result_resent",
                "reply_invalid",
            ]
        );
    }

    /// Catches: a count that is not kept per counter, or a listing that names a counter
    /// never bumped.
    #[test]
    fn counts_are_kept_per_counter() {
        let mut counters = Counters::default();
        counters.bump(Counter::ResultResent);
        counters.bump(Counter::StartRepeated);
        counters.bump(Counter::ResultResent);
        assert_eq!(counters.get(Counter::ResultResent), 2);
        assert_eq!(counters.get(Counter::ReplyInvalid), 0);
        let listed: Vec<(Counter, u64)> = counters.iter().collect();
        assert_eq!(
            listed,
            [(Counter::StartRepeated, 1), (Counter::ResultResent, 2)]
        );
    }
}
