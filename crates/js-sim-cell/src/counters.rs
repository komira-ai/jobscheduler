//! What an agent node, or an elastic pool's cloud, counts: how a test, or a scenario's
//! witness, shows that the situation it is about happened.

use std::collections::BTreeMap;

/// One thing an agent node counts, or (the last four) the cell's elastic pool's cloud
/// ([`crate::Cloud::counters`]).
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
    /// The agent was cut off from the server while it ran a run that no hello or
    /// heartbeat it had sent listed: the server cannot have seen it listed. Bumped by
    /// [`crate::actions::partition_agents`] at the cut.
    CutWithUnlistedRun,
    /// A heartbeat answered 409 `unknown-host`: the server had forgotten the host.
    UnknownHost,
    /// The cloud gave the scaler no answer to an acquire (`Acquired::Unknown`): the call
    /// may have landed.
    AcquireLost,
    /// The cloud deferred an acquire (`Acquired::Deferred`): a 429, or an acquire of an id
    /// whose answer was lost and is not yet resolved.
    AcquireThrottled,
    /// The cloud refused an acquire (`Acquired::Refused`), whatever the code.
    AcquireRefused,
    /// The cloud preempted a machine it was running ([`crate::Cell::preempt_machine`]).
    MachinePreempted,
}

impl Counter {
    /// Every counter, in order.
    pub const ALL: [Counter; 11] = [
        Counter::StartRepeated,
        Counter::StartAfterStop,
        Counter::StartDroppedLate,
        Counter::ResultResent,
        Counter::ReplyInvalid,
        Counter::CutWithUnlistedRun,
        Counter::UnknownHost,
        Counter::AcquireLost,
        Counter::AcquireThrottled,
        Counter::AcquireRefused,
        Counter::MachinePreempted,
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
            Counter::CutWithUnlistedRun => "cut_with_unlisted_run",
            Counter::UnknownHost => "unknown_host",
            Counter::AcquireLost => "acquire_lost",
            Counter::AcquireThrottled => "acquire_throttled",
            Counter::AcquireRefused => "acquire_refused",
            Counter::MachinePreempted => "machine_preempted",
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

    /// The counts of `all` added together.
    pub(crate) fn sum<'a>(all: impl IntoIterator<Item = &'a Counters>) -> Counters {
        let mut total = Counters::default();
        for counters in all {
            total.absorb(counters);
        }
        total
    }

    /// Adds every count of `other` to this one's.
    pub(crate) fn absorb(&mut self, other: &Counters) {
        for (counter, count) in other.iter() {
            *self.0.entry(counter).or_insert(0) += count;
        }
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
                "cut_with_unlisted_run",
                "unknown_host",
                "acquire_lost",
                "acquire_throttled",
                "acquire_refused",
                "machine_preempted",
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
