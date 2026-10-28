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
//! The input types are the usage crate's own. [`NoUsage`] is the sink that records nothing, and
//! [`NoKeys`] the key source that knows no run's key.

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::sync::Arc;

use js_types::{HostId, LeaseId};

/// What the server knows of a node's capacity, for the sink's checks on a reading that
/// claims more than the machine has.
pub use js_usage::Limits as HostLimits;
/// One closed wall minute of one node.
pub use js_usage::NodeMinute as NodeRow;
pub use js_usage::{
    BadKey, ClockIn, DropReason, Fit, KeyProfile, LoadIn, MAX_KEY_LEN, MinuteIn, Observed,
    ProfileCurves, ProfileKey, RunMinute, RunSeries,
};

/// A short fixed name for a drop reason, for logs.
pub fn drop_name(reason: DropReason) -> &'static str {
    match reason {
        DropReason::NoBaseline => "no-baseline",
        DropReason::EpochChanged => "epoch-changed",
        DropReason::SeqNotAbove => "seq-not-above",
        DropReason::ClockBack => "clock-back",
        DropReason::CounterBack => "counter-back",
        DropReason::ImpliedCoresOverFourXThreads => "implied-cores-over-4x-threads",
        DropReason::MemoryOverFourXCapacity => "memory-over-4x-capacity",
    }
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

    /// At most `limit` rows of `host` for the minutes `from` to `to` inclusive, oldest
    /// first.
    fn node_rows(
        &self,
        host: &HostId,
        from: u64,
        to: u64,
        limit: NonZeroUsize,
    ) -> Result<Vec<NodeRow>, UsageFault>;

    /// What is stored for `key` (its run count, its fit once there are enough runs, the
    /// day it was last used), or `None` for a key never seen.
    fn key_profile(&self, key: &ProfileKey) -> Result<Option<KeyProfile>, UsageFault>;

    /// The learned curves, for admission to read as its `CurveSource`; `None` for a sink
    /// that learns none.
    fn curves(&self) -> Option<Arc<ProfileCurves>> {
        None
    }

    /// What the sink has counted.
    fn counters(&self) -> SinkCounters;
}

/// The sink that records nothing: every reading is taken and forgotten, every read is
/// empty.
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

    fn node_rows(
        &self,
        _: &HostId,
        _: u64,
        _: u64,
        _: NonZeroUsize,
    ) -> Result<Vec<NodeRow>, UsageFault> {
        Ok(Vec::new())
    }

    fn key_profile(&self, _: &ProfileKey) -> Result<Option<KeyProfile>, UsageFault> {
        Ok(None)
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
        assert_eq!(
            sink.node_rows(&host, 0, 9, NonZeroUsize::MIN),
            Ok(Vec::new())
        );
        assert_eq!(sink.key_profile(&end.key), Ok(None));
        assert!(sink.curves().is_none());
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
            EpochChanged,
            SeqNotAbove,
            ClockBack,
            CounterBack,
            ImpliedCoresOverFourXThreads,
            MemoryOverFourXCapacity,
        ];
        let names: std::collections::BTreeSet<_> = all.iter().map(|r| drop_name(*r)).collect();
        assert_eq!(names.len(), all.len());
    }
}
