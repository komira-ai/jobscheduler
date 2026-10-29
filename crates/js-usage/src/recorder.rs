//! The pure state machine behind the node series and the lease minute feed.
//!
//! [`Recorder::observe`] takes one reading per node at a time, judges it against the last
//! one accepted, and answers with what to do: the node's CPU and working set go into the
//! wall minute the caller names, and the lease minutes it carries come back deduplicated.
//! It reads no clock and holds only ordered collections; time is the `now_wall_minute`
//! argument.
//!
//! Time bases never mix. A reading's `mono_usec` is the node's own monotonic clock and is
//! compared only with the same node's earlier readings, so two nodes whose clocks differ
//! by hours do not interact. The wall minute is the server's, supplied by the caller.
//!
//! A reading is refused, never an error, when it cannot be trusted (see [`DropReason`]);
//! the refusal is counted in [`Counters`]. The state is in memory: after a restart the
//! first reading of each node only sets a baseline.
//!
//! A caller that keeps no node series builds the recorder with [`Recorder::leases_only`]:
//! the lease side behaves the same, but node minutes are never stored, so none pile up
//! and [`Recorder::node_minutes_due`] has nothing to drain.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

use js_types::{HostId, LeaseId};

use crate::input::{LoadIn, MinuteIn, TAIL_MINUTE};
use crate::minute::NodeMinute;

/// Microseconds in a minute of a node's monotonic clock.
pub const MINUTE_USEC: u64 = 60_000_000;

/// A reading implying more than this many times the node's threads of CPU, or more than
/// this many times its memory, is a lying node.
pub const IMPLAUSIBLE_FACTOR: u64 = 4;

/// How many wall minutes a remembered `(lease, minute)` or lease clock is kept without
/// being seen again. The agent drops an unsent minute after ten minutes, so a resend
/// always arrives well inside it.
pub const SEEN_RETENTION_MINUTES: u64 = 30;

/// The most `(lease, minute)` records remembered per node: 64 minutes of 96 leases, the
/// most a node's reading can carry. Beyond it the least recently seen are forgotten.
pub const MAX_SEEN_PER_HOST: usize = 96 * 64;

/// The most lease clocks remembered per node, four times what one reading carries.
pub const MAX_CLOCKS_PER_HOST: usize = 4 * 96;

/// The most ended leases remembered, so that a late reading naming one is refused. Past
/// it the least recently ended are forgotten, and a reading that late is taken as new.
pub const MAX_ENDED_LEASES: usize = 4096;

/// Why a reading was not used.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DropReason {
    /// The first reading of a node since this recorder started: it only sets a baseline.
    NoBaseline,
    /// The epoch differs from the one the node was recorded under: the old state is
    /// discarded and this reading is the new baseline.
    EpochChanged,
    /// The sequence number is not above the last accepted one (a resend, or out of order).
    SeqNotAbove,
    /// The node's monotonic clock did not advance. The reading becomes the new baseline.
    ClockBack,
    /// The cumulative CPU counter went back. The reading becomes the new baseline.
    CounterBack,
    /// The CPU between two readings implies more than four times the node's threads.
    ImpliedCoresOverFourXThreads,
    /// The working set is more than four times the node's memory.
    MemoryOverFourXCapacity,
}

/// What [`Recorder::observe`] did with a reading.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Observed {
    /// The reading went into the node's minute.
    Accepted,
    /// The reading was not used, for this reason.
    Dropped(DropReason),
}

/// The capacity the server knows for a node, for the checks against a lying agent. A zero
/// field means unknown: that check is skipped, never failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Hardware threads.
    pub threads: u32,
    /// Memory in bytes.
    pub memory_bytes: u64,
}

/// What the recorder has counted since it was made.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    /// Readings not used, by reason.
    pub dropped: BTreeMap<DropReason, u64>,
    /// Lease minutes that repeated one already seen and added nothing.
    pub minutes_deduped: u64,
    /// Lease minutes past [`TAIL_MINUTE`], which a valid agent never sends.
    pub minutes_invalid: u64,
    /// Lease minutes naming a lease already reported ended by [`Recorder::lease_ended`];
    /// they are left out, so an ended lease is not counted again.
    pub minutes_after_end: u64,
    /// Remembered `(lease, minute)` records forgotten for age or for the cap.
    pub seen_evicted: u64,
    /// Remembered lease clocks forgotten for age or for the cap.
    pub clocks_evicted: u64,
}

impl Counters {
    /// Readings not used, all reasons together.
    #[must_use]
    pub fn dropped_total(&self) -> u64 {
        self.dropped.values().sum()
    }
}

#[derive(Clone, Copy)]
struct Reading {
    seq: u64,
    mono: u64,
    cpu: u64,
}

/// The sums of the accepted readings of one wall minute.
#[derive(Default)]
struct Accum {
    cpu_usec: u128,
    mono_usec: u128,
    ws_max: u64,
    leases: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Figures {
    mean: u32,
    peak: u32,
    ws: u64,
}

impl Figures {
    /// The larger of each field: how repeats of the tail minute merge.
    fn max(self, other: Figures) -> Figures {
        Figures {
            mean: self.mean.max(other.mean),
            peak: self.peak.max(other.peak),
            ws: self.ws.max(other.ws),
        }
    }
}

struct Seen {
    figures: Figures,
    touched: u64,
}

struct Clock {
    started: u64,
    touched: u64,
}

struct HostState {
    epoch: u64,
    last: Reading,
    clocks: BTreeMap<LeaseId, Clock>,
    seen: BTreeMap<(LeaseId, u16), Seen>,
    minutes: BTreeMap<u64, Accum>,
}

/// The state of every node a server hears from.
#[derive(Default)]
pub struct Recorder {
    leases_only: bool,
    hosts: BTreeMap<HostId, HostState>,
    ended: BTreeSet<LeaseId>,
    ended_order: VecDeque<LeaseId>,
    counters: Counters,
}

impl Recorder {
    /// A recorder that has heard from no node.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A recorder that has heard from no node and keeps lease series only: the run
    /// series, lease clocks and checks work as in [`Recorder::new`], but a reading's CPU
    /// and working set are not stored as node minutes, so [`Recorder::node_minutes_due`]
    /// is always empty and memory does not grow with node minutes. For a caller that
    /// keeps no node series and so would never drain them.
    #[must_use]
    pub fn leases_only() -> Self {
        Self {
            leases_only: true,
            ..Self::default()
        }
    }

    /// Judges `load`, the reading of node `host` under `epoch`, received in wall minute
    /// `now_wall_minute`. Returns what was done with the reading and the lease minutes
    /// that are new: a repeat of a `(lease, minute)` already seen is left out, except that
    /// a repeat of the tail minute is merged by maximum per field and returned when it
    /// raised any.
    ///
    /// The lease minutes of a first reading and of an epoch change are returned (they
    /// describe leases, not the counter that has no baseline); those of a reading refused
    /// as untrustworthy are not.
    pub fn observe(
        &mut self,
        host: &HostId,
        epoch: u64,
        load: &LoadIn,
        limits: Limits,
        now_wall_minute: u64,
    ) -> (Observed, Vec<MinuteIn>) {
        let reading = Reading {
            seq: load.seq,
            mono: load.mono_usec,
            cpu: load.cpu_usage_usec,
        };
        let leases_only = self.leases_only;
        let counters = &mut self.counters;
        let ended = &self.ended;
        let state = match self.hosts.entry(host.clone()) {
            Entry::Vacant(slot) => {
                let state = slot.insert(HostState::new(epoch, reading));
                let minutes = state.take(load, now_wall_minute, ended, counters);
                return (refuse(counters, DropReason::NoBaseline), minutes);
            }
            Entry::Occupied(slot) => slot.into_mut(),
        };
        if state.epoch != epoch {
            *state = HostState::new(epoch, reading);
            return (
                refuse(counters, DropReason::EpochChanged),
                state.take(load, now_wall_minute, ended, counters),
            );
        }
        let no_minutes = |counters: &mut Counters, reason| (refuse(counters, reason), Vec::new());
        if reading.seq <= state.last.seq {
            return no_minutes(counters, DropReason::SeqNotAbove);
        }
        if reading.mono <= state.last.mono {
            state.last = reading;
            return no_minutes(counters, DropReason::ClockBack);
        }
        if reading.cpu < state.last.cpu {
            state.last = reading;
            return no_minutes(counters, DropReason::CounterBack);
        }
        let d_mono = u128::from(reading.mono - state.last.mono);
        let d_cpu = u128::from(reading.cpu - state.last.cpu);
        let factor = u128::from(IMPLAUSIBLE_FACTOR);
        if limits.threads != 0 && d_cpu > d_mono * u128::from(limits.threads) * factor {
            return no_minutes(counters, DropReason::ImpliedCoresOverFourXThreads);
        }
        if limits.memory_bytes != 0
            && u128::from(load.working_set_bytes) > u128::from(limits.memory_bytes) * factor
        {
            return no_minutes(counters, DropReason::MemoryOverFourXCapacity);
        }
        state.last = reading;
        let running: BTreeSet<&LeaseId> = load
            .covered
            .iter()
            .chain(load.clocks.iter().map(|clock| &clock.lease))
            .collect();
        if !leases_only {
            let acc = state.minutes.entry(now_wall_minute).or_default();
            acc.cpu_usec += d_cpu;
            acc.mono_usec += d_mono;
            acc.ws_max = acc.ws_max.max(load.working_set_bytes);
            acc.leases = acc
                .leases
                .max(u32::try_from(running.len()).unwrap_or(u32::MAX));
        }
        (
            Observed::Accepted,
            state.take(load, now_wall_minute, ended, counters),
        )
    }

    /// The wall minutes before `now_wall_minute` that have readings, as rows, removed from
    /// the recorder. Rows come host by host, minute by minute.
    pub fn node_minutes_due(&mut self, now_wall_minute: u64) -> Vec<(HostId, NodeMinute)> {
        let mut due = Vec::new();
        for (host, state) in &mut self.hosts {
            let later = state.minutes.split_off(&now_wall_minute);
            for (minute, acc) in std::mem::replace(&mut state.minutes, later) {
                let milli = acc
                    .cpu_usec
                    .saturating_mul(1000)
                    .checked_div(acc.mono_usec)
                    .unwrap_or(0);
                due.push((
                    host.clone(),
                    NodeMinute {
                        minute,
                        cpu_millis: u32::try_from(milli).unwrap_or(u32::MAX),
                        ws_max_bytes: acc.ws_max,
                        leases: acc.leases,
                    },
                ));
            }
        }
        due
    }

    /// How many node minutes are held, not yet handed out by
    /// [`Recorder::node_minutes_due`]; always 0 for a [`Recorder::leases_only`] recorder.
    #[doc(hidden)]
    #[must_use]
    pub fn held_node_minutes(&self) -> usize {
        self.hosts.values().map(|state| state.minutes.len()).sum()
    }

    /// Forgets a node, as when it leaves the pool. Its open minutes are lost.
    pub fn forget_host(&mut self, host: &HostId) {
        self.hosts.remove(host);
    }

    /// Reports that `lease` has ended: its result was accepted and its grace beats are
    /// over. On every host the recorder forgets the `(lease, minute)` records it holds for
    /// it at once, without waiting for them to age out.
    ///
    /// The lease is then remembered as ended, and a later reading that still carries a
    /// minute naming it adds nothing: the minute is left out of what
    /// [`Recorder::observe`] returns and counted in [`Counters::minutes_after_end`]. Were
    /// it not, the forgotten records would take a late repeat for new and count the lease
    /// twice. Lease clocks are not touched. At most [`MAX_ENDED_LEASES`] ended leases are
    /// remembered, the least recently ended forgotten first; a reading naming a lease
    /// forgotten that way is judged as any other. Ending a lease already ended, or one the
    /// recorder holds nothing for, forgets nothing more (the latter is still remembered as
    /// ended). The same on a [`Recorder::leases_only`] recorder.
    pub fn lease_ended(&mut self, lease: &LeaseId) {
        for state in self.hosts.values_mut() {
            let keys: Vec<(LeaseId, u16)> = state
                .seen
                .range((*lease, 0)..=(*lease, u16::MAX))
                .map(|(key, _)| *key)
                .collect();
            for key in keys {
                state.seen.remove(&key);
            }
        }
        if self.ended.insert(*lease) {
            self.ended_order.push_back(*lease);
            let excess = self.ended_order.len().saturating_sub(MAX_ENDED_LEASES);
            for oldest in self.ended_order.drain(..excess) {
                self.ended.remove(&oldest);
            }
        }
    }

    /// How many `(lease, minute)` records are remembered for `lease`, over all nodes.
    #[doc(hidden)]
    #[must_use]
    pub fn remembered_minutes(&self, lease: &LeaseId) -> usize {
        self.hosts
            .values()
            .map(|state| state.seen.range((*lease, 0)..=(*lease, u16::MAX)).count())
            .sum()
    }

    /// How many ended leases are remembered; at most [`MAX_ENDED_LEASES`].
    #[doc(hidden)]
    #[must_use]
    pub fn remembered_ended(&self) -> usize {
        self.ended.len()
    }

    /// Whole minutes since `lease` started, on `host`'s own clock at its latest accepted
    /// reading, rounded down; 0 for a lease with no known start or a start after the
    /// reading.
    #[must_use]
    pub fn elapsed_minute(&self, host: &HostId, lease: &LeaseId) -> u32 {
        let Some(state) = self.hosts.get(host) else {
            return 0;
        };
        let Some(clock) = state.clocks.get(lease) else {
            return 0;
        };
        let elapsed = state.last.mono.saturating_sub(clock.started) / MINUTE_USEC;
        u32::try_from(elapsed).unwrap_or(u32::MAX)
    }

    /// What has been counted.
    #[must_use]
    pub fn counters(&self) -> &Counters {
        &self.counters
    }
}

fn refuse(counters: &mut Counters, reason: DropReason) -> Observed {
    *counters.dropped.entry(reason).or_default() += 1;
    Observed::Dropped(reason)
}

impl HostState {
    fn new(epoch: u64, last: Reading) -> Self {
        Self {
            epoch,
            last,
            clocks: BTreeMap::new(),
            seen: BTreeMap::new(),
            minutes: BTreeMap::new(),
        }
    }

    /// Records the lease clocks of `load` and returns its lease minutes that are new.
    fn take(
        &mut self,
        load: &LoadIn,
        now: u64,
        ended: &BTreeSet<LeaseId>,
        counters: &mut Counters,
    ) -> Vec<MinuteIn> {
        for clock in &load.clocks {
            let entry = Clock {
                started: clock.started_mono_usec,
                touched: now,
            };
            self.clocks.insert(clock.lease, entry);
        }
        counters.clocks_evicted += trim(&mut self.clocks, now, MAX_CLOCKS_PER_HOST, |c| c.touched);
        let mut fresh = Vec::new();
        for minute in &load.minutes {
            if minute.minute > TAIL_MINUTE {
                counters.minutes_invalid += 1;
                continue;
            }
            if ended.contains(&minute.lease) && minute.minute > TAIL_MINUTE {
                counters.minutes_after_end += 1;
                continue;
            }
            let figures = Figures {
                mean: minute.mean_cores_milli,
                peak: minute.peak_window_milli,
                ws: minute.working_set_max_bytes,
            };
            match self.seen.entry((minute.lease, minute.minute)) {
                Entry::Vacant(slot) => {
                    slot.insert(Seen {
                        figures,
                        touched: now,
                    });
                    fresh.push(minute.clone());
                }
                Entry::Occupied(mut slot) => {
                    let seen = slot.get_mut();
                    seen.touched = now;
                    // Only the tail minute stands for several; any other repeat is a resend.
                    let merged = if minute.minute == TAIL_MINUTE {
                        figures.max(seen.figures)
                    } else {
                        seen.figures
                    };
                    if merged == seen.figures {
                        counters.minutes_deduped += 1;
                    } else {
                        seen.figures = merged;
                        fresh.push(MinuteIn {
                            mean_cores_milli: merged.mean,
                            peak_window_milli: merged.peak,
                            working_set_max_bytes: merged.ws,
                            ..minute.clone()
                        });
                    }
                }
            }
        }
        counters.seen_evicted += trim(&mut self.seen, now, MAX_SEEN_PER_HOST, |s| s.touched);
        fresh
    }
}

/// Forgets the entries of `map` not touched within [`SEEN_RETENTION_MINUTES`] of `now`,
/// then the least recently touched (by key at a tie) until at most `cap` remain. Returns
/// how many it forgot.
fn trim<K: Ord + Copy, V>(
    map: &mut BTreeMap<K, V>,
    now: u64,
    cap: usize,
    touched: impl Fn(&V) -> u64,
) -> u64 {
    let before = map.len();
    map.retain(|_, value| touched(value).saturating_add(SEEN_RETENTION_MINUTES) >= now);
    if map.len() > cap {
        let mut order: Vec<(u64, K)> = map.iter().map(|(key, v)| (touched(v), *key)).collect();
        order.sort();
        for (_, key) in order.into_iter().take(map.len() - cap) {
            map.remove(&key);
        }
    }
    u64::try_from(before - map.len()).unwrap_or(u64::MAX)
}
