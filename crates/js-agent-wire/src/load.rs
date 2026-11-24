//! What a host measured about itself: [`NodeLoad`], the optional per-minute load a
//! [`crate::Heartbeat`] may carry.
//!
//! The load is telemetry. A bad one never costs the beat around it, at two levels:
//! decoding drops a structurally bad load (the heartbeat reads as if it carried none) or
//! a structurally bad record, and [`NodeLoad::vet`] drops a record that breaks a rule and
//! fails only for a bad scalar, which the server answers by discarding the load, not the
//! beat. [`crate::Heartbeat::validate`] never looks at the load.
//!
//! Every number is an integer, so the types are `Eq`. The lease ids on the wire are the
//! root form `term-seq` (see [`lease_parts`]), the one grammar this crate owns.
//!
//! The two per-second lists are plain numbers, not records: an element that is not a number
//! of its type is dropped as malformed, and [`NodeLoad::vet`] empties a pressure list that
//! has a value above 1000, since a list with holes pairs with nothing.
//!
//! Minute indexes run from 0 to [`limits::TAIL_MINUTE`]; the last one means "that minute
//! or later", so a receiver merges repeats of it by taking the larger value of each field.

use std::collections::BTreeSet;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

use crate::check::{self, At};
use crate::error::ValidationError;
use crate::limits;
use crate::refusal;

/// What a host measured, as of one reading of its monotonic clock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NodeLoad {
    /// The reading's sequence number, required. State rules (above the last one seen)
    /// are the receiver's.
    pub seq: u64,
    /// The agent's monotonic clock at the reading, in microseconds, required.
    pub mono_usec: u64,
    /// Cumulative CPU time of the cgroup that holds every action, in microseconds,
    /// required. A Mac, which has no such group, sends the whole host's busy time here as in
    /// `host_cpu_usec`.
    pub cpu_usage_usec: u64,
    /// The whole host's cumulative CPU busy time, in microseconds (`/proc/stat`, on a Mac the
    /// host's CPU load ticks: every process of the host, the actions and everything else).
    /// Optional: an agent of an
    /// earlier release does not send it, and a receiver then reads the host's CPU from
    /// `cpu_usage_usec` ([`NodeLoad::node_cpu_usec`]). An agent that sends it sends it in
    /// every reading of its process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_cpu_usec: Option<u64>,
    /// Memory in use by the actions, in bytes (`memory.current - inactive_file`),
    /// required. At most [`limits::MAX_MEMORY_BYTES`]. A Mac, which has no actions group,
    /// sends the whole host's non-file memory here as in `host_nonfile_bytes`.
    pub working_set_bytes: u64,
    /// Active file cache, in bytes. At most [`limits::MAX_MEMORY_BYTES`].
    #[serde(default)]
    pub active_file_bytes: u64,
    /// The whole host's non-file memory, in bytes: `MemTotal - MemFree - (Buffers + Cached -
    /// Shmem)` of `/proc/meminfo`, every process's anonymous and kernel memory and the host's
    /// shared memory and tmpfs, and none of its page cache, which the kernel can drop (on a Mac
    /// its anonymous pages that are not purgeable, its wired pages and the compressor's). At most
    /// [`limits::MAX_MEMORY_BYTES`]. Optional: an agent of an earlier release does not send it,
    /// nor one that could not read the file, and a receiver then reads the host's memory from
    /// `working_set_bytes` ([`NodeLoad::node_memory_bytes`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_nonfile_bytes: Option<u64>,
    /// Whether the actions cgroup has a CPU limit.
    #[serde(default)]
    pub actions_cpu_limited: bool,
    /// Lease ids that ran for the whole window since the previous reading. At most
    /// [`limits::MAX_LOAD_LEASES`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub covered: Vec<String>,
    /// When each lease started, on the agent's monotonic clock. At most
    /// [`limits::MAX_LOAD_LEASES`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clocks: Vec<LeaseClock>,
    /// Closed lease-minutes. At most [`limits::MAX_LOAD_MINUTES`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub minutes: Vec<LeaseMinute>,
    /// Busy CPU of the whole host (every CPU's busy time in `/proc/stat` or a Mac's CPU load,
    /// not the actions cgroup's) over each interval between two of the agent's one-second samples, in
    /// thousandths of a core (`busy_usec * 1000 / time`), oldest first. At most [`limits::MAX_LOAD_SECONDS`]. Telemetry only: nothing in
    /// placement or sizing reads it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub second_cpu_millis: Vec<u32>,
    /// The share of each of those intervals in which some task of the host waited for a CPU
    /// (the `some` line of `/proc/pressure/cpu`), in thousandths, at most 1000,
    /// oldest first. At most [`limits::MAX_LOAD_SECONDS`]. Empty when the host has no
    /// pressure files (a Mac has none), while `second_cpu_millis` is not. The two lists pair by position
    /// only when they have the same length.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub second_cpu_pressure_some_permille: Vec<u16>,
    /// What decoding and [`NodeLoad::vet`] dropped. Never on the wire; part of `Eq`.
    #[serde(skip)]
    pub dropped: LoadDrops,
}

/// One closed minute of one lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LeaseMinute {
    /// The lease, in the form [`lease_parts`] reads.
    pub lease_id: String,
    /// The minute since the lease's start, 0 to [`limits::TAIL_MINUTE`] (the last means
    /// that minute or later).
    pub minute: u16,
    /// Mean CPU over the observed span, in thousandths of a core. Not above the peak.
    pub cpu_mean_millis: u32,
    /// The highest CPU rate of any window in the minute, in thousandths of a core.
    pub cpu_peak_window_millis: u32,
    /// The largest working set seen in the minute, in bytes. At most
    /// [`limits::MAX_MEMORY_BYTES`].
    pub ws_max_bytes: u64,
}

/// When one lease started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LeaseClock {
    /// The lease, in the form [`lease_parts`] reads.
    pub lease_id: String,
    /// The start on the agent's monotonic clock, in microseconds; not after
    /// [`NodeLoad::mono_usec`].
    pub started_mono_usec: u64,
}

/// How many records decoding and [`NodeLoad::vet`] dropped, by reason.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LoadDrops {
    /// Records that did not decode, and lists that were not arrays.
    pub malformed: u32,
    /// Records that broke a rule.
    pub invalid: u32,
    /// Records that repeated an earlier one.
    pub duplicate: u32,
    /// Records past the cap of their list.
    pub over_cap: u32,
}

impl LoadDrops {
    /// Every drop, summed without overflow.
    pub fn total(&self) -> u32 {
        self.malformed
            .saturating_add(self.invalid)
            .saturating_add(self.duplicate)
            .saturating_add(self.over_cap)
    }
}

/// Reads a lease id of the root form `term-seq`: two canonical decimal `u64` joined by a
/// dash, with no sign, space, leading zero (except `0` itself) or other character.
/// Returns `(term, seq)`, or `None` for anything else. This is the only parser of a lease
/// id on the wire.
pub fn lease_parts(text: &str) -> Option<(u64, u64)> {
    if text.len() > limits::MAX_LEASE_REF_LEN {
        return None;
    }
    let (term, seq) = text.split_once('-')?;
    let term = term.parse::<u64>().ok()?;
    let seq = seq.parse::<u64>().ok()?;
    (format!("{term}-{seq}") == text).then_some((term, seq))
}

/// What to do with one record of a list.
enum Verdict {
    Keep,
    Invalid,
    Duplicate,
}

/// Cuts `items` to `cap`, then keeps the records `judge` keeps, counting the rest.
fn sweep<T>(
    items: &mut Vec<T>,
    cap: usize,
    drops: &mut LoadDrops,
    mut judge: impl FnMut(&T) -> Verdict,
) {
    let excess = items.len().saturating_sub(cap);
    items.truncate(cap);
    drops.over_cap = drops
        .over_cap
        .saturating_add(u32::try_from(excess).unwrap_or(u32::MAX));
    items.retain(|item| match judge(item) {
        Verdict::Keep => true,
        Verdict::Invalid => {
            drops.invalid = drops.invalid.saturating_add(1);
            false
        }
        Verdict::Duplicate => {
            drops.duplicate = drops.duplicate.saturating_add(1);
            false
        }
    });
}

fn lease_ok(id: &str) -> bool {
    lease_parts(id).is_some()
}

impl NodeLoad {
    /// The counter a receiver reads the host's CPU busy time from: `host_cpu_usec` when the
    /// agent sends it, else `cpu_usage_usec` (an agent of an earlier release, whose only
    /// counter is its actions cgroup's).
    #[must_use]
    pub fn node_cpu_usec(&self) -> u64 {
        self.host_cpu_usec.unwrap_or(self.cpu_usage_usec)
    }

    /// The memory a receiver reads as the host's in use, and the active file pages in it:
    /// `host_nonfile_bytes` and none when the agent sends it (the whole host's non-file
    /// memory, which holds no page cache), else `working_set_bytes` and `active_file_bytes`
    /// (an agent of an earlier release, whose only figure is its actions cgroup's).
    #[must_use]
    pub fn node_memory_bytes(&self) -> (u64, u64) {
        self.host_nonfile_bytes
            .map_or((self.working_set_bytes, self.active_file_bytes), |held| {
                (held, 0)
            })
    }

    /// Drops the records that break a rule and counts them in [`NodeLoad::dropped`].
    ///
    /// Fails only for a scalar out of range (`working_set_bytes`, `active_file_bytes`,
    /// `host_nonfile_bytes`);
    /// the receiver then discards the load and keeps the heartbeat. Counters (`seq`,
    /// `mono_usec`, `cpu_usage_usec`, `host_cpu_usec`) are unbounded here: whether one goes
    /// backwards is
    /// the receiver's state to judge. Calling it twice changes nothing the second time.
    pub fn vet(&mut self) -> Result<(), ValidationError> {
        let at = At::new("heartbeat.load", refusal::INVALID_HEARTBEAT);
        let max = limits::MAX_MEMORY_BYTES;
        check::range(
            at.field("heartbeat.load.working_set_bytes"),
            self.working_set_bytes,
            0,
            max,
        )?;
        check::range(
            at.field("heartbeat.load.active_file_bytes"),
            self.active_file_bytes,
            0,
            max,
        )?;
        if let Some(held) = self.host_nonfile_bytes {
            check::range(at.field("heartbeat.load.host_nonfile_bytes"), held, 0, max)?;
        }
        let mono = self.mono_usec;
        sweep(
            &mut self.second_cpu_millis,
            limits::MAX_LOAD_SECONDS,
            &mut self.dropped,
            |_| Verdict::Keep,
        );
        // A pressure value is a share of the interval, so one above 1000 is a broken
        // sender, and the list goes whole: a list with a hole in it pairs with nothing.
        let hole = self
            .second_cpu_pressure_some_permille
            .iter()
            .any(|p| *p > 1000);
        if hole {
            self.second_cpu_pressure_some_permille.clear();
            self.dropped.invalid = self.dropped.invalid.saturating_add(1);
        }
        sweep(
            &mut self.second_cpu_pressure_some_permille,
            limits::MAX_LOAD_SECONDS,
            &mut self.dropped,
            |_| Verdict::Keep,
        );
        let mut seen = BTreeSet::new();
        sweep(
            &mut self.minutes,
            limits::MAX_LOAD_MINUTES,
            &mut self.dropped,
            |m| {
                if !lease_ok(&m.lease_id)
                    || m.minute > limits::TAIL_MINUTE
                    || m.ws_max_bytes > max
                    || m.cpu_mean_millis > m.cpu_peak_window_millis
                {
                    Verdict::Invalid
                } else if !seen.insert((m.lease_id.clone(), m.minute)) {
                    Verdict::Duplicate
                } else {
                    Verdict::Keep
                }
            },
        );
        let mut seen = BTreeSet::new();
        sweep(
            &mut self.clocks,
            limits::MAX_LOAD_LEASES,
            &mut self.dropped,
            |c| {
                if !lease_ok(&c.lease_id) || c.started_mono_usec > mono {
                    Verdict::Invalid
                } else if !seen.insert(c.lease_id.clone()) {
                    Verdict::Duplicate
                } else {
                    Verdict::Keep
                }
            },
        );
        let mut seen = BTreeSet::new();
        sweep(
            &mut self.covered,
            limits::MAX_LOAD_LEASES,
            &mut self.dropped,
            |id| {
                if !lease_ok(id) {
                    Verdict::Invalid
                } else if !seen.insert(id.clone()) {
                    Verdict::Duplicate
                } else {
                    Verdict::Keep
                }
            },
        );
        Ok(())
    }

    /// Reads a load from JSON text already parsed, or `None` when it is not an object or
    /// lacks a required scalar. Bad records are dropped and counted.
    fn from_value(value: &Value) -> Option<Self> {
        let object = value.as_object()?;
        let (Some(seq), Some(mono_usec), Some(cpu_usage_usec), Some(working_set_bytes)) = (
            number(object, "seq"),
            number(object, "mono_usec"),
            number(object, "cpu_usage_usec"),
            number(object, "working_set_bytes"),
        ) else {
            return None;
        };
        let mut dropped = LoadDrops::default();
        let covered = list(
            object,
            "covered",
            limits::MAX_LOAD_LEASES,
            &mut dropped,
            |item| item.as_str().map(str::to_owned),
        );
        let clocks = list(
            object,
            "clocks",
            limits::MAX_LOAD_LEASES,
            &mut dropped,
            read_clock,
        );
        let minutes = list(
            object,
            "minutes",
            limits::MAX_LOAD_MINUTES,
            &mut dropped,
            read_minute,
        );
        let second_cpu_millis = list(
            object,
            "second_cpu_millis",
            limits::MAX_LOAD_SECONDS,
            &mut dropped,
            |item| item.as_u64().and_then(|n| u32::try_from(n).ok()),
        );
        let second_cpu_pressure_some_permille = list(
            object,
            "second_cpu_pressure_some_permille",
            limits::MAX_LOAD_SECONDS,
            &mut dropped,
            |item| item.as_u64().and_then(|n| u16::try_from(n).ok()),
        );
        Some(Self {
            seq,
            mono_usec,
            cpu_usage_usec,
            host_cpu_usec: number(object, "host_cpu_usec"),
            working_set_bytes,
            active_file_bytes: number(object, "active_file_bytes").unwrap_or_default(),
            host_nonfile_bytes: number(object, "host_nonfile_bytes"),
            actions_cpu_limited: object
                .get("actions_cpu_limited")
                .and_then(Value::as_bool)
                .unwrap_or_default(),
            covered,
            clocks,
            minutes,
            second_cpu_millis,
            second_cpu_pressure_some_permille,
            dropped,
        })
    }
}

fn number(object: &Map<String, Value>, key: &str) -> Option<u64> {
    object.get(key).and_then(Value::as_u64)
}

fn text(object: &Map<String, Value>, key: &str) -> Option<String> {
    object.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn small<T: TryFrom<u64>>(object: &Map<String, Value>, key: &str) -> Option<T> {
    number(object, key).and_then(|n| T::try_from(n).ok())
}

fn read_clock(item: &Value) -> Option<LeaseClock> {
    let object = item.as_object()?;
    let (Some(lease_id), Some(started_mono_usec)) = (
        text(object, "lease_id"),
        number(object, "started_mono_usec"),
    ) else {
        return None;
    };
    Some(LeaseClock {
        lease_id,
        started_mono_usec,
    })
}

fn read_minute(item: &Value) -> Option<LeaseMinute> {
    let object = item.as_object()?;
    let (
        Some(lease_id),
        Some(minute),
        Some(cpu_mean_millis),
        Some(cpu_peak_window_millis),
        Some(ws_max_bytes),
    ) = (
        text(object, "lease_id"),
        small::<u16>(object, "minute"),
        small::<u32>(object, "cpu_mean_millis"),
        small::<u32>(object, "cpu_peak_window_millis"),
        number(object, "ws_max_bytes"),
    )
    else {
        return None;
    };
    Some(LeaseMinute {
        lease_id,
        minute,
        cpu_mean_millis,
        cpu_peak_window_millis,
        ws_max_bytes,
    })
}

/// Reads the array at `key` element by element. An absent key is an empty list; a value
/// that is not an array is an empty list and one `malformed`; an element that does not
/// decode is dropped as `malformed`; every element after the first `cap` decoded ones is
/// dropped as `over_cap` without being read, so memory is bounded by `cap`.
fn list<T>(
    object: &Map<String, Value>,
    key: &str,
    cap: usize,
    drops: &mut LoadDrops,
    read: impl Fn(&Value) -> Option<T>,
) -> Vec<T> {
    let Some(value) = object.get(key) else {
        return Vec::new();
    };
    let Some(items) = value.as_array() else {
        drops.malformed = drops.malformed.saturating_add(1);
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in items {
        if out.len() >= cap {
            drops.over_cap = drops.over_cap.saturating_add(1);
        } else if let Some(record) = read(item) {
            out.push(record);
        } else {
            drops.malformed = drops.malformed.saturating_add(1);
        }
    }
    out
}

/// Decodes a heartbeat's `load` leniently: anything that is not a well-formed load reads
/// as `None`, so a bad load never fails the heartbeat around it.
pub(crate) fn lenient_load<'de, D>(deserializer: D) -> Result<Option<NodeLoad>, D::Error>
where
    D: Deserializer<'de>,
{
    Value::deserialize(deserializer).map(|value| NodeLoad::from_value(&value))
}
