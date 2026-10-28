//! The seam between the server and whatever measures usage: [`UsageSink`], the plain
//! values that cross it, and [`KeySource`].
//!
//! The server does the transport work (a heartbeat's load is checked, its lease ids
//! parsed, the node's identity taken from the registry's answer) and hands plain values
//! to a sink. The sink owns the arithmetic and the storage: which readings are lies,
//! how a reading becomes a node minute, how minutes are kept and pruned, and how a run
//! becomes a cost curve. Nothing in the seam names a wire type, a store or a clock.
//!
//! Time is an argument: every call that needs the time is given the current wall minute
//! (minutes since the Unix epoch, from the server's [`crate::SharedClock`]).
//!
//! The types mirror the fields of the usage crate's inputs, so that crate's types
//! replace these by a rename. [`NoUsage`] is the sink that records nothing, and
//! [`NoKeys`] the key source that knows no run's key.

use std::collections::BTreeMap;
use std::fmt;

use js_types::{HostId, LeaseId};

/// One reading of a node, as plain values: the wire form checked, with lease ids parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadIn {
    /// The reading's sequence number.
    pub seq: u64,
    /// The node's monotonic clock at the reading, in microseconds.
    pub mono_usec: u64,
    /// Cumulative CPU time of the node's actions cgroup, in microseconds.
    pub cpu_usage_usec: u64,
    /// Memory in use by the actions, in bytes.
    pub working_set_bytes: u64,
    /// Whether the actions cgroup has a CPU limit.
    pub actions_cpu_limited: bool,
    /// The leases that ran for the whole window since the previous reading.
    pub covered: Vec<LeaseId>,
    /// Closed lease-minutes.
    pub minutes: Vec<MinuteIn>,
    /// When each lease started, on the node's monotonic clock.
    pub clocks: Vec<ClockIn>,
}

/// One closed minute of one lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MinuteIn {
    /// The lease.
    pub lease: LeaseId,
    /// The minute since the lease's start; the last index (63) means "or later".
    pub minute: u16,
    /// Mean CPU over the observed span, in thousandths of a core.
    pub mean_cores_milli: u32,
    /// The highest CPU rate of any window in the minute, in thousandths of a core.
    pub peak_window_milli: u32,
    /// The largest working set seen in the minute, in bytes.
    pub working_set_max_bytes: u64,
}

/// When one lease started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClockIn {
    /// The lease.
    pub lease: LeaseId,
    /// The start on the node's monotonic clock, in microseconds.
    pub started_mono_usec: u64,
}

/// What the server knows of a node's capacity, for the sink's checks on a reading that
/// claims more than the machine has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostLimits {
    /// The processor threads the pool books for a host.
    pub threads: u32,
    /// The memory the pool books for a host, in bytes.
    pub memory_bytes: u64,
}

/// Why a sink did not take a reading. The node is not at fault for most of these (the
/// first reading after a restart has nothing to compare with); none of them is an error
/// to the node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DropReason {
    /// The first reading of an epoch: there is no earlier one to measure against.
    NoBaseline,
    /// The sequence number did not rise above the last accepted one.
    SeqNotAbove,
    /// The CPU counter went backwards.
    CounterBack,
    /// The monotonic clock went backwards.
    ClockBack,
    /// The CPU the reading implies is over the limit the node's threads allow.
    ImpliedCoresOverLimit,
    /// The working set is over the limit the node's memory allows.
    MemoryOverLimit,
    /// The epoch changed under the reading.
    EpochChanged,
}

impl DropReason {
    /// A short fixed name, for logs.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoBaseline => "no-baseline",
            Self::SeqNotAbove => "seq-not-above",
            Self::CounterBack => "counter-back",
            Self::ClockBack => "clock-back",
            Self::ImpliedCoresOverLimit => "implied-cores-over-limit",
            Self::MemoryOverLimit => "memory-over-limit",
            Self::EpochChanged => "epoch-changed",
        }
    }
}

/// What a sink did with a reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Observed {
    /// The reading was taken.
    Accepted,
    /// The reading was not taken, and why.
    Dropped(DropReason),
}

/// A sink's answer to a reading: what it did, and the lease minutes it had not seen
/// before (a resent minute is not fresh).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    /// What was done with the reading.
    pub observed: Observed,
    /// The lease minutes of the reading that are new.
    pub fresh: Vec<MinuteIn>,
}

/// One closed wall minute of one node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeRow {
    /// The wall minute, in minutes since the Unix epoch.
    pub minute: u64,
    /// CPU used over the minute, in thousandths of a core.
    pub cpu_millis: u32,
    /// The largest working set in the minute, in bytes.
    pub ws_max_bytes: u64,
    /// How many leases ran on the node.
    pub leases: u32,
}

/// A run's measured series: `(minute, mean cores in thousandths, working set maximum in
/// bytes)` for each minute since the run's start, in minute order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RunSeries {
    /// The minutes.
    pub minutes: Vec<(u16, u32, u64)>,
}

/// A run that ended cleanly, with the key of the work it did and what it measured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunEnd {
    /// What kind of work the run was.
    pub key: ProfileKey,
    /// The lease the run was under.
    pub lease: LeaseId,
    /// What it measured.
    pub series: RunSeries,
}

/// The key a work profile is kept under: 1 to 128 characters of `[0-9a-z._-]`, the
/// first not a dot, so that it is one path segment that names nothing but itself.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProfileKey(String);

/// Text that is not a [`ProfileKey`]. It does not keep the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("not a valid key")]
pub struct BadKey;

/// The longest [`ProfileKey`], in bytes.
pub const MAX_KEY_LEN: usize = 128;

impl ProfileKey {
    /// The key `text` spells, if it is one.
    pub fn parse(text: &str) -> Result<Self, BadKey> {
        let sound = (1..=MAX_KEY_LEN).contains(&text.len())
            && text.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
            && text.chars().all(is_key_char);
        match sound {
            true => Ok(Self(text.to_string())),
            false => Err(BadKey),
        }
    }

    /// The key as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Whether `c` may appear in a key.
fn is_key_char(c: char) -> bool {
    matches!(c, 'a'..='z' | '0'..='9' | '.' | '_' | '-')
}

impl fmt::Display for ProfileKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a sink counts, for [`crate::Running::usage_counters`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SinkCounters {
    /// Readings not taken, by reason.
    pub dropped: BTreeMap<DropReason, u64>,
    /// Lease minutes that arrived twice and were taken once.
    pub minutes_deduped: u64,
}

/// A sink's store could not do what was asked. The text says what failed and never holds
/// text a client sent.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("usage store: {0}")]
pub struct UsageFault(pub String);

/// What the server asks of whatever measures usage.
///
/// Every call may be made from any thread, and the server calls them on the pool for
/// blocking calls, never on an async worker; an implementation may block on its store.
/// A call that fails costs usage data only: the server counts and logs it and goes on.
pub trait UsageSink: Send + Sync {
    /// Takes a reading of `host`, at `epoch` of its session. `limits` is the capacity
    /// the pool books for it.
    fn observe(
        &self,
        host: &HostId,
        epoch: u64,
        load: &LoadIn,
        limits: HostLimits,
        now_minute: u64,
    ) -> Observation;

    /// Learns from a run that ended cleanly.
    fn run_ended(&self, end: &RunEnd, now_minute: u64) -> Result<(), UsageFault>;

    /// Closes the wall minutes that are over at `now_minute`, writes every closed row
    /// that is not yet written, and returns the rows closed by this call. A row not
    /// written stays buffered for the next call.
    fn flush(&self, now_minute: u64) -> Result<Vec<(HostId, NodeRow)>, UsageFault>;

    /// Removes what is past its retention at `now_minute`.
    fn prune(&self, now_minute: u64) -> Result<(), UsageFault>;

    /// What the sink has counted.
    fn counters(&self) -> SinkCounters;
}

/// The sink that records nothing: every reading is taken and forgotten.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoUsage;

impl UsageSink for NoUsage {
    fn observe(&self, _: &HostId, _: u64, _: &LoadIn, _: HostLimits, _: u64) -> Observation {
        Observation {
            observed: Observed::Accepted,
            fresh: Vec::new(),
        }
    }

    fn run_ended(&self, _: &RunEnd, _: u64) -> Result<(), UsageFault> {
        Ok(())
    }

    fn flush(&self, _: u64) -> Result<Vec<(HostId, NodeRow)>, UsageFault> {
        Ok(Vec::new())
    }

    fn prune(&self, _: u64) -> Result<(), UsageFault> {
        Ok(())
    }

    fn counters(&self) -> SinkCounters {
        SinkCounters::default()
    }
}

/// Which key of work a run was. The server has no key of its own for a run: the owner of
/// admission supplies one.
pub trait KeySource: Send + Sync {
    /// The key of the work run `run_id` did, or `None` when it has none (no profile is
    /// learned from it).
    fn key_of(&self, run_id: &str) -> Option<ProfileKey>;
}

/// The key source that knows no run's key: nothing is learned from any run.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoKeys;

impl KeySource for NoKeys {
    fn key_of(&self, _: &str) -> Option<ProfileKey> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts the grammar of a key: 1 to 128 characters of lowercase letters, digits,
    /// dot, underscore and dash, the first not a dot or a separator. It catches a key
    /// that could be a path trick (`.`, `..`, `%2e`, a slash), an uppercase one that
    /// names the same thing twice, a key one character over the cap, and the empty key.
    #[test]
    fn a_key_is_one_plain_segment() {
        let longest = "a".repeat(MAX_KEY_LEN);
        let over = "a".repeat(MAX_KEY_LEN + 1);
        for good in ["a", "0", "a.b_c-d", "x86-64.v3", longest.as_str()] {
            assert_eq!(ProfileKey::parse(good).unwrap().as_str(), good);
            assert_eq!(ProfileKey::parse(good).unwrap().to_string(), good);
        }
        for bad in [
            "",
            ".",
            "..",
            ".a",
            "-a",
            "_a",
            "A",
            "aB",
            "a/b",
            "/a",
            "a b",
            "%2e",
            "a%2e",
            "é",
            over.as_str(),
        ] {
            assert_eq!(ProfileKey::parse(bad), Err(BadKey), "{bad:?}");
        }
        assert_eq!(BadKey.to_string(), "not a valid key");
    }

    /// Asserts that the sink that records nothing takes every reading, returns no closed
    /// rows, and the key source that knows nothing names no key. It catches a
    /// default sink that errors (and so counts a fault on every beat) or invents rows.
    #[test]
    fn the_empty_sink_and_the_empty_key_source_do_nothing() {
        let host = HostId::new("h").unwrap();
        let load = LoadIn {
            seq: 1,
            mono_usec: 1,
            cpu_usage_usec: 1,
            working_set_bytes: 1,
            actions_cpu_limited: false,
            covered: Vec::new(),
            minutes: Vec::new(),
            clocks: Vec::new(),
        };
        let limits = HostLimits {
            threads: 1,
            memory_bytes: 1,
        };
        let sink = NoUsage;
        let seen = sink.observe(&host, 1, &load, limits, 0);
        assert_eq!(seen.observed, Observed::Accepted);
        assert!(seen.fresh.is_empty());
        let end = RunEnd {
            key: ProfileKey::parse("k").unwrap(),
            lease: LeaseId::new(1, 1),
            series: RunSeries::default(),
        };
        assert_eq!(sink.run_ended(&end, 0), Ok(()));
        assert_eq!(sink.flush(0), Ok(Vec::new()));
        assert_eq!(sink.prune(0), Ok(()));
        assert_eq!(sink.counters(), SinkCounters::default());
        assert_eq!(NoKeys.key_of("run-1-1"), None);
    }

    /// Asserts that every drop reason has its own fixed name. It catches two reasons
    /// sharing a name, which would merge their counts in the log.
    #[test]
    fn every_drop_reason_has_its_own_name() {
        use DropReason::*;
        let all = [
            NoBaseline,
            SeqNotAbove,
            CounterBack,
            ClockBack,
            ImpliedCoresOverLimit,
            MemoryOverLimit,
            EpochChanged,
        ];
        let names: std::collections::BTreeSet<_> = all.iter().map(|r| r.as_str()).collect();
        assert_eq!(names.len(), all.len());
    }
}
