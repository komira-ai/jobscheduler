//! What keeps a whole-machine request off a worker ([`Bar`]): the back-off of a hold it
//! gave up, and the hard bars a caller sets from outside ([`Scheduler::bar_whole`]).
//!
//! [`Scheduler::bar_whole`]: crate::Scheduler::bar_whole

use std::collections::BTreeMap;

use js_types::sched::{FarmTime, WorkerId};

/// Why, and until when, a whole-machine request is kept off a worker
/// ([`crate::Scheduler::whole_barred`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bar {
    /// The back-off after a hold given up at its deadline, until the instant given
    /// (exclusive): the request makes no mark on the worker, and takes it when it is
    /// quiet only if no other worker could run the request (as [`crate::Fate::avoid`]
    /// is). This is the bar a give-up sets.
    Backoff(FarmTime),
    /// A hard bar until the instant given (exclusive): the worker is neither marked for
    /// the request nor granted to it, quiet or not, whatever the other workers.
    Until(FarmTime),
    /// A hard bar that never ends.
    ForLife,
}

impl Bar {
    /// Whether the bar still holds at `now`.
    pub(in crate::scheduler) const fn holds_at(self, now: FarmTime) -> bool {
        match self {
            Self::Backoff(until) | Self::Until(until) => now.as_millis() < until.as_millis(),
            Self::ForLife => true,
        }
    }

    /// Whether the bar keeps the worker from a quiet grant too: every bar but a back-off.
    pub(in crate::scheduler) const fn is_hard(self) -> bool {
        !matches!(self, Self::Backoff(_))
    }

    /// When the bar ends (exclusive), the end of time for a bar for life.
    pub(in crate::scheduler) const fn end(self) -> FarmTime {
        match self {
            Self::Backoff(until) | Self::Until(until) => until,
            Self::ForLife => FarmTime::from_millis(u64::MAX),
        }
    }

    /// The strength of a bar: a hard bar over a back-off, then the later end.
    const fn strength(self) -> (bool, u64) {
        (self.is_hard(), self.end().as_millis())
    }
}

/// Sets `bar` on `worker` in `bars` at `now`: the worker keeps the stronger of the bar it
/// has and `bar` (hard over a back-off, then the later end), or takes `bar` if the one it
/// has has ended.
pub(in crate::scheduler) fn set_bar(
    bars: &mut BTreeMap<WorkerId, Bar>,
    worker: WorkerId,
    bar: Bar,
    now: FarmTime,
) {
    let old = bars.get(&worker).copied().unwrap_or(bar);
    let kept = if old.holds_at(now) && old.strength() >= bar.strength() {
        old
    } else {
        bar
    };
    bars.insert(worker, kept);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(ms: u64) -> FarmTime {
        FarmTime::from_millis(ms)
    }

    /// Catches: an end counted inclusive, a bar for life that ends, and a back-off taken
    /// for a hard bar.
    #[test]
    fn a_bar_holds_until_its_end_exclusive_and_only_a_backoff_is_soft() {
        assert!(Bar::Backoff(t(10)).holds_at(t(9)));
        assert!(!Bar::Backoff(t(10)).holds_at(t(10)));
        assert!(Bar::Until(t(10)).holds_at(t(9)));
        assert!(!Bar::Until(t(10)).holds_at(t(10)));
        assert!(Bar::ForLife.holds_at(t(u64::MAX)));
        assert!(!Bar::Backoff(t(10)).is_hard());
        assert!(Bar::Until(t(10)).is_hard());
        assert!(Bar::ForLife.is_hard());
        assert_eq!(Bar::ForLife.end(), t(u64::MAX));
    }

    /// Catches: a weaker bar replacing a stronger one that still holds (a back-off over a
    /// hard bar, an earlier end over a later one), and an ended bar kept over a new one.
    #[test]
    fn a_worker_keeps_the_stronger_bar_until_it_ends() {
        let a = WorkerId::new("a");
        let mut bars = BTreeMap::new();
        let set = |bars: &mut BTreeMap<WorkerId, Bar>, bar, now| {
            set_bar(bars, a.clone(), bar, t(now));
            bars[&a]
        };
        assert_eq!(set(&mut bars, Bar::Backoff(t(50)), 0), Bar::Backoff(t(50)));
        assert_eq!(set(&mut bars, Bar::Until(t(20)), 0), Bar::Until(t(20)));
        assert_eq!(set(&mut bars, Bar::Backoff(t(90)), 10), Bar::Until(t(20)));
        assert_eq!(set(&mut bars, Bar::Until(t(15)), 10), Bar::Until(t(20)));
        assert_eq!(set(&mut bars, Bar::Until(t(30)), 10), Bar::Until(t(30)));
        assert_eq!(set(&mut bars, Bar::Backoff(t(90)), 30), Bar::Backoff(t(90)));
        assert_eq!(set(&mut bars, Bar::ForLife, 40), Bar::ForLife);
        assert_eq!(set(&mut bars, Bar::Until(t(u64::MAX)), 40), Bar::ForLife);
    }
}
