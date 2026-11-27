//! Whether a worker's CPU signal can be believed: the plausibility check of the
//! placement-by-load design (section "Lying or stuck node, restart").
//!
//! A lease's CPU time arrives only with its result, so a long lease would land hours of CPU
//! in one short span. The check therefore counts only the leases that were admitted and
//! ended inside a span of readings, and compares their summed CPU time with the growth of
//! the node's CPU counter over the same span. The counter holds every other process of the
//! host too, so on a healthy node its growth can only exceed that sum. The span is the
//! newest window, extended back one window at a time until the leases in it used
//! [`SPAN_CPU_USEC`] (60 core-seconds), and never longer than [`SPAN_MAX_USEC`] (an hour of
//! the agent's clock). A span that never holds that much leaves the window *unchecked*, not
//! failed, and the time unchecked is counted. Growth below half the sum marks the signal
//! implausible, and the worker is charged by prediction until a later comparison agrees.
//!
//! A worker that held no lease through a window and whose counter says it was busy on more
//! than half its cores, for [`IDLE_HIGH_WINDOWS`] such windows in a row, is marked the same
//! way, until a window with no lease reads half its cores or less.
//!
//! Everything here is plain arithmetic on values the ledger hands in; time is the
//! scheduler's [`FarmTime`] (when a lease was admitted, when it ended, when a reading
//! arrived) and the agent's monotonic clock (how long a span is), never a clock read.

use std::collections::{BTreeMap, VecDeque};

use js_types::sched::{FarmTime, LeaseId};

use super::LoadCounters;

/// The lease CPU time a span must hold before the check compares it with the counter: 60
/// core-seconds, in microseconds.
pub(crate) const SPAN_CPU_USEC: u64 = 60_000_000;

/// The longest span, in microseconds of the agent's clock: one hour.
pub(crate) const SPAN_MAX_USEC: u64 = 3_600_000_000;

/// Windows in a row in which a worker with no lease reports more than half its cores before
/// its signal is marked implausible.
pub(crate) const IDLE_HIGH_WINDOWS: u32 = 3;

/// An accepted reading, as the check keeps it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Mark {
    /// When the reading arrived, on the scheduler's clock.
    arrived: FarmTime,
    /// The agent's clock when it took the reading.
    mono_usec: u64,
    /// The node's cumulative CPU counter.
    cpu_usec: u64,
}

impl Mark {
    /// A reading that arrived at `arrived`, taken at `mono_usec` on the agent's clock with the
    /// counter at `cpu_usec`.
    pub(crate) const fn new(arrived: FarmTime, mono_usec: u64, cpu_usec: u64) -> Self {
        Self {
            arrived,
            mono_usec,
            cpu_usec,
        }
    }
}

/// A lease that ended, with the CPU time its result reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Run {
    admitted: FarmTime,
    /// When it left the ledger, or when its CPU time arrived if that was later: by then the
    /// run had used all of it.
    ended: FarmTime,
    cpu_usec: u64,
}

/// A lease that left the ledger before its CPU time arrived.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Awaiting {
    admitted: FarmTime,
}

/// What the comparison of one window found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verdict {
    /// The counter grew by at least half the leases' CPU time.
    Agrees,
    /// It grew by less.
    Disagrees,
    /// No span of at most an hour held enough lease CPU time to compare.
    Unchecked,
}

/// The check's state for one worker.
#[derive(Clone, Debug, Default)]
pub(crate) struct Check {
    /// The accepted readings of the worker's current session, oldest first, back at most
    /// [`SPAN_MAX_USEC`] from the newest.
    marks: VecDeque<Mark>,
    /// The leases admitted after the oldest mark that ended with their CPU time known.
    runs: Vec<Run>,
    /// The leases admitted after the oldest mark that ended with their CPU time unknown.
    awaiting: BTreeMap<LeaseId, Awaiting>,
    /// Some lease was on the ledger at some moment since the newest mark.
    lease_in_window: bool,
    /// Windows in a row with no lease that read more than half the worker's cores.
    idle_high: u32,
    /// The comparison of lease CPU with the counter found the signal implausible and no later
    /// comparison agreed.
    lease_cpu_implausible: bool,
    /// The idle rule marked the signal and no later idle window read half the cores or less.
    idle_implausible: bool,
    /// The agent-clock time of the windows in a row, up to the newest, that were unchecked.
    unchecked_usec: u64,
}

impl Check {
    /// Whether the signal is marked implausible, by either rule.
    pub(crate) const fn implausible(&self) -> bool {
        self.lease_cpu_implausible || self.idle_implausible
    }

    /// Whether the comparison of lease CPU with the counter marked the signal.
    pub(crate) const fn lease_cpu_implausible(&self) -> bool {
        self.lease_cpu_implausible
    }

    /// Whether the idle rule marked the signal.
    pub(crate) const fn idle_implausible(&self) -> bool {
        self.idle_implausible
    }

    /// The agent-clock time of the windows in a row, up to the newest, that were unchecked.
    pub(crate) const fn unchecked_usec(&self) -> u64 {
        self.unchecked_usec
    }

    /// A lease was put on the ledger: the window under way is not idle.
    pub(crate) const fn lease_present(&mut self) {
        self.lease_in_window = true;
    }

    /// `lease` left the ledger at `now`. Only an action lease admitted by placement (not one
    /// recovered, whose start is not known) can be counted: `admitted` is `None` for the
    /// others, and they are not kept; nor is any lease while the worker has no accepted
    /// reading (a worker that sends none would otherwise keep every lease for good). `cpu_usec` is its CPU time if it arrived already.
    pub(crate) fn ended(
        &mut self,
        lease: LeaseId,
        admitted: Option<FarmTime>,
        cpu_usec: Option<u64>,
        now: FarmTime,
    ) {
        // With no mark yet, no span can hold the lease: its run began before any reading.
        let Some(admitted) = admitted.filter(|_| !self.marks.is_empty()) else {
            return;
        };
        match cpu_usec {
            Some(cpu_usec) => self.runs.push(Run {
                admitted,
                ended: now,
                cpu_usec,
            }),
            None => {
                self.awaiting.insert(lease, Awaiting { admitted });
            }
        }
    }

    /// The CPU time of `lease`, which left the ledger before it arrived, arrived at `now`.
    /// Answers whether the check was waiting for it.
    pub(crate) fn late_cpu(&mut self, lease: LeaseId, cpu_usec: u64, now: FarmTime) -> bool {
        let Some(waiting) = self.awaiting.remove(&lease) else {
            return false;
        };
        self.runs.push(Run {
            admitted: waiting.admitted,
            ended: now,
            cpu_usec,
        });
        true
    }

    /// A new session: its counter says nothing about the old one's, so the spans start
    /// again. A signal marked implausible stays marked until a comparison or an idle window
    /// of the new session clears it: a restarted agent is believed no sooner than a running
    /// one, and the unchecked time runs on.
    pub(crate) fn new_session(&mut self) {
        self.marks.clear();
        self.runs.clear();
        self.awaiting.clear();
        self.idle_high = 0;
    }

    /// The first accepted reading of a session, which gives no window. `leases` is whether
    /// the ledger holds a lease now.
    pub(crate) fn baseline(&mut self, mark: Mark, leases: bool) {
        self.marks.push_back(mark);
        self.lease_in_window = leases;
    }

    /// An accepted reading that closes a window: `busy_millis` is the cores the counter says
    /// were busy over it, `cpu_capacity` the worker's cores, both in millicores, and `leases`
    /// whether the ledger holds a lease now. Applies the idle rule and the comparison, and
    /// counts what they found in `counters`.
    pub(crate) fn window(
        &mut self,
        mark: Mark,
        (busy_millis, cpu_capacity): (u32, u64),
        leases: bool,
        counters: &mut LoadCounters,
    ) {
        let idle = !self.lease_in_window;
        self.lease_in_window = leases;
        self.idle_rule(idle, u64::from(busy_millis), cpu_capacity, counters);
        let before = self.marks.back().map_or(mark.mono_usec, |m| m.mono_usec);
        self.marks.push_back(mark);
        self.prune(mark);
        match self.compare(mark) {
            Verdict::Agrees => {
                counters.signal_checked_windows += 1;
                self.lease_cpu_implausible = false;
                self.unchecked_usec = 0;
            }
            Verdict::Disagrees => {
                counters.signal_checked_windows += 1;
                counters.signal_implausible_lease_cpu += u64::from(!self.lease_cpu_implausible);
                self.lease_cpu_implausible = true;
                self.unchecked_usec = 0;
            }
            Verdict::Unchecked => {
                let length = mark.mono_usec.saturating_sub(before);
                counters.signal_unchecked_windows += 1;
                counters.signal_unchecked_usec =
                    counters.signal_unchecked_usec.saturating_add(length);
                self.unchecked_usec = self.unchecked_usec.saturating_add(length);
            }
        }
    }

    /// A worker with no lease all window that reads more than half its cores, for
    /// [`IDLE_HIGH_WINDOWS`] windows in a row, is marked; an idle window at half or below
    /// clears the mark. A window with a lease says nothing either way and ends the run.
    fn idle_rule(&mut self, idle: bool, busy: u64, capacity: u64, counters: &mut LoadCounters) {
        if !idle {
            self.idle_high = 0;
            return;
        }
        if capacity > 0 && busy.saturating_mul(2) > capacity {
            self.idle_high = self.idle_high.saturating_add(1);
            if self.idle_high >= IDLE_HIGH_WINDOWS && !self.idle_implausible {
                self.idle_implausible = true;
                counters.signal_implausible_idle += 1;
            }
        } else {
            self.idle_high = 0;
            self.idle_implausible = false;
        }
    }

    /// Lets go of the marks older than the longest span from `newest`, the newest mark, and
    /// of the leases no span from the oldest mark left can hold (admitted at or before it).
    fn prune(&mut self, newest: Mark) {
        while self
            .marks
            .front()
            .is_some_and(|m| newest.mono_usec.saturating_sub(m.mono_usec) > SPAN_MAX_USEC)
        {
            self.marks.pop_front();
        }
        let oldest = self.marks.front().map_or(newest.arrived, |m| m.arrived);
        self.runs.retain(|r| r.admitted > oldest);
        self.awaiting.retain(|_, a| a.admitted > oldest);
    }

    /// Compares the CPU time of the leases admitted after an earlier mark and ended before
    /// `newest` (the last of the marks) arrived with the counter's growth since that earlier
    /// mark, taking the nearest earlier mark whose span holds [`SPAN_CPU_USEC`].
    ///
    /// A lease admitted at the very time a reading arrived, or ended at the very time the
    /// newest did, is left out of that span: the order of the two inside one moment is not
    /// known, so some of its CPU may lie outside the span.
    fn compare(&self, newest: Mark) -> Verdict {
        let mut runs: Vec<&Run> = self
            .runs
            .iter()
            .filter(|r| r.ended < newest.arrived)
            .collect();
        runs.sort_by_key(|r| std::cmp::Reverse(r.admitted));
        let mut runs = runs.into_iter().peekable();
        let mut sum: u64 = 0;
        for start in self.marks.iter().rev().skip(1) {
            while let Some(run) = runs.next_if(|r| r.admitted > start.arrived) {
                sum = sum.saturating_add(run.cpu_usec);
            }
            if sum >= SPAN_CPU_USEC {
                let growth = newest.cpu_usec.saturating_sub(start.cpu_usec);
                return if growth.saturating_mul(2) < sum {
                    Verdict::Disagrees
                } else {
                    Verdict::Agrees
                };
            }
        }
        Verdict::Unchecked
    }
}

#[cfg(test)]
#[path = "plausible_tests.rs"]
mod tests;
