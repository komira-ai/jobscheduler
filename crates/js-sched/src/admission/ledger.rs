//! One worker's load as admission sees it: the readings it sent, the cost fixed for each
//! lease placed on it, and the sums the predicate reads.
//!
//! Everything here is integer arithmetic on plain values; time is the scheduler's
//! [`FarmTime`], never a clock.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use js_types::sched::usage::{LeaseCost, LeaseMinute, NodeLoad};
use js_types::sched::{FarmTime, LeaseId, Resources};

use super::horizon;
use super::plausible::{Check, Mark};
use super::profile::{Lanes, Profile};
use super::{LoadConfig, LoadCounters, charge};
use crate::refusal::{Refusal, RefusalKind};

/// A reading is dropped when it implies more cores than this many times the worker's
/// capacity, or more memory than this many times its capacity.
const IMPLAUSIBLE_FACTOR: u64 = 4;

/// A minute of the agent's clock, in microseconds.
const MINUTE_USEC: u64 = 60_000_000;

/// The least a lease is charged on a worker that has never sent an accepted reading (a
/// Mac, a native or VM node): nothing there measures a light population, so only the
/// predictions protect it. A worker that has measured keeps the sub-core charge of the
/// estimate (250 m at least) even when its signal has gone stale.
pub(crate) const UNMEASURED_CPU_FLOOR_MILLIS: u64 = 1000;

/// One lease on the ledger.
#[derive(Clone, Debug)]
pub(super) struct Entry {
    pub(super) cost: LeaseCost,
    /// A VM lease: it runs outside the actions cgroup, so its booked cost is added to
    /// every sum and no reading covers it.
    pub(super) vm: bool,
    /// The newest accepted reading lists it as running for the whole window.
    pub(super) covered: bool,
    /// It was on the ledger when the newest accepted reading was taken in, so the reading's
    /// window may hold some of its CPU: at minute 0 its cores are added to the window before
    /// the newest, not to the measurement. Every covered lease is seen.
    pub(super) seen: bool,
    /// How it is charged over time, when it is charged by curve or to an expected life.
    pub(super) profile: Option<Arc<Profile>>,
    /// Its age in whole minutes as of the newest accepted reading (its own clock); 0 for
    /// a lease no reading has given a clock.
    pub(super) age: u32,
    /// The newest accepted reading gives it a clock.
    pub(super) clocked: bool,
    /// Its age is known: a lease no reading covers is as old as it was placed (0), a
    /// covered one needs the clock. A covered lease without one is charged flat.
    pub(super) aged: bool,
    /// The working set of its newest closed minute when that was above what its curve asks
    /// for from that minute on, else 0: the lease is charged what it uses until a minute
    /// says it uses less. Only a lease charged by a memory curve can have one.
    pub(super) ws_over: u64,
    /// The newest minute record taken in.
    seen_minute: Option<u32>,
    /// Some minute of it misfit its CPU curve / memory curve.
    cpu_misfit: bool,
    mem_misfit: bool,
    /// When placement admitted it; `None` for a lease taken in otherwise (recovered), whose
    /// start the plausibility check cannot place in a span.
    admitted: Option<FarmTime>,
    /// The CPU time its result reported, in microseconds, when it arrived before the lease
    /// left the ledger.
    cpu_used: Option<u64>,
}

impl Entry {
    fn new(cost: LeaseCost, vm: bool, profile: Option<Arc<Profile>>) -> Self {
        Self {
            cost,
            vm,
            covered: false,
            seen: false,
            profile,
            age: 0,
            clocked: false,
            aged: true,
            ws_over: 0,
            seen_minute: None,
            cpu_misfit: false,
            mem_misfit: false,
            admitted: None,
            cpu_used: None,
        }
    }

    /// Takes in a closed minute of the lease; counts the minutes that misfit.
    fn observe(&mut self, minute: &LeaseMinute, counters: &mut LoadCounters) {
        if self.seen_minute.is_some_and(|seen| minute.minute <= seen) {
            return;
        }
        self.seen_minute = Some(minute.minute);
        self.ws_over = 0;
        let Some(p) = self.profile.as_deref() else {
            return;
        };
        if p.cpu_misfit(minute.minute, minute.cpu_mean_millis) {
            self.cpu_misfit = true;
            counters.misfit_cpu_minutes += 1;
        }
        if p.mem_misfit(minute.minute, minute.ws_max_bytes) {
            self.mem_misfit = true;
            self.ws_over = minute.ws_max_bytes;
            counters.misfit_memory_minutes += 1;
        }
    }
}

/// A lease taken off the ledger: what the shape guard needs to judge its run.
#[derive(Clone, Debug)]
pub(crate) struct Ended {
    pub(crate) profile: Option<Arc<Profile>>,
    pub(crate) cpu_misfit: bool,
    pub(crate) mem_misfit: bool,
}

/// The sums of the predicted charges on a worker.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Totals {
    pub(crate) cpu_covered: u64,
    pub(crate) cpu_uncovered: u64,
    /// Of `cpu_uncovered`, the cores of the leases placed after the newest accepted reading
    /// was taken in: no reading has seen them.
    pub(crate) cpu_unseen: u64,
    pub(crate) mem_covered: u64,
    pub(crate) mem_uncovered: u64,
    pub(crate) vm_cpu: u64,
    pub(crate) vm_mem: u64,
    /// How many action leases the ledger holds.
    pub(crate) actions: usize,
    /// The per-slot charges of the leases charged over time.
    pub(super) lanes: Lanes,
}

impl Totals {
    fn shift(&mut self, entry: &Entry, up: bool) {
        self.lanes.shift(entry, up);
        let c = charge(&entry.cost);
        let (cpu, mem) = if entry.vm {
            (&mut self.vm_cpu, &mut self.vm_mem)
        } else if entry.covered {
            (&mut self.cpu_covered, &mut self.mem_covered)
        } else {
            (&mut self.cpu_uncovered, &mut self.mem_uncovered)
        };
        if up {
            *cpu = cpu.saturating_add(c.cpu_millis);
            *mem = mem.saturating_add(c.memory_bytes);
        } else {
            *cpu = cpu.saturating_sub(c.cpu_millis);
            *mem = mem.saturating_sub(c.memory_bytes);
        }
        if !entry.vm && !entry.seen {
            self.cpu_unseen = if up {
                self.cpu_unseen.saturating_add(c.cpu_millis)
            } else {
                self.cpu_unseen.saturating_sub(c.cpu_millis)
            };
        }
        if !entry.vm {
            if up {
                self.actions += 1;
            } else {
                self.actions -= 1;
            }
        }
    }

    /// Predicted memory of every action lease, covered or not.
    pub(crate) const fn mem_all(&self) -> u64 {
        self.mem_covered.saturating_add(self.mem_uncovered)
    }

    /// Predicted cores of every action lease, covered or not.
    pub(crate) const fn cpu_all(&self) -> u64 {
        self.cpu_covered.saturating_add(self.cpu_uncovered)
    }

    /// What the covered leases are charged `slot` minutes ahead (the `H+` slot included):
    /// the floor `f` on the cores of those charged flat, plus the charge of those charged
    /// by curve. At slot 0 with no lease charged over time this is `f * sum p(covered)`.
    pub(crate) fn cpu_covered_at(&self, slot: usize, cfg: &LoadConfig) -> u64 {
        let l = &self.lanes;
        let flat = self
            .cpu_covered
            .saturating_sub(l.timed_cpu_cov_p)
            .saturating_add(l.cpu_cov_p.at(slot));
        cfg.floor_of(flat).saturating_add(l.cpu_cov_c.at(slot))
    }

    /// What the uncovered leases are charged `slot` minutes ahead.
    pub(crate) fn cpu_uncovered_at(&self, slot: usize) -> u64 {
        let l = &self.lanes;
        self.cpu_uncovered
            .saturating_sub(l.timed_cpu_unc_p)
            .saturating_add(l.cpu_unc.at(slot))
    }

    /// What the leases no accepted reading has seen are charged `slot` minutes ahead: the
    /// part of [`Totals::cpu_uncovered_at`] that is placed after the newest reading.
    pub(crate) fn cpu_unseen_at(&self, slot: usize) -> u64 {
        let l = &self.lanes;
        self.cpu_unseen
            .saturating_sub(l.timed_cpu_unseen_p)
            .saturating_add(l.cpu_unseen.at(slot))
    }

    /// Memory the action leases are charged `slot` minutes ahead, covered or not.
    pub(crate) fn mem_all_at(&self, slot: usize) -> u64 {
        let l = &self.lanes;
        self.mem_all()
            .saturating_sub(l.timed_mem_cov)
            .saturating_sub(l.timed_mem_unc)
            .saturating_add(l.mem_all.at(slot))
    }

    /// Memory the uncovered leases are charged `slot` minutes ahead.
    pub(crate) fn mem_uncovered_at(&self, slot: usize) -> u64 {
        let l = &self.lanes;
        self.mem_uncovered
            .saturating_sub(l.timed_mem_unc)
            .saturating_add(l.mem_unc.at(slot))
    }

    /// Whether some lease is charged over time.
    pub(crate) const fn has_timed(&self) -> bool {
        self.lanes.timed > 0
    }
}

/// The newest accepted reading and what it gives.
#[derive(Clone, Debug)]
struct Readings {
    last: NodeLoad,
    /// The farm time the newest reading arrived at: the only time base of freshness.
    arrived: FarmTime,
    /// Cores demanded over the last two windows ([`NodeLoad::demand_millicores_since`]),
    /// newest first, in millicores. Known once two consecutive readings are accepted.
    windows: [Option<u32>; 2],
}

/// The cold leases admitted in one window.
#[derive(Clone, Copy, Debug, Default)]
struct ColdWindow {
    index: u64,
    admitted: u32,
}

/// What the predicate's left side is made of, for one worker at one moment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Left {
    /// Cores the worker is charged, in millicores, without the new lease.
    pub(crate) cpu: u64,
    /// Memory the worker is charged, in bytes, without the new lease.
    pub(crate) mem: u64,
    /// No usable measurement: every lease is charged its full predicted cost.
    pub(crate) fallback: bool,
    /// The least CPU any lease is charged on this worker: [`UNMEASURED_CPU_FLOOR_MILLIS`]
    /// (within the worker's cores) on a worker that never sent an accepted reading, else 0.
    /// The sum in `cpu` already holds it for the leases on the ledger; the new lease takes
    /// it through [`Left::charged_cpu`].
    pub(crate) floor_cpu: u64,
    /// The measured cores ([`WorkerLoad::measured`]), in millicores, the left side holds (0
    /// when it is charged by prediction alone: `fallback` tells that from a host measured idle).
    pub(crate) measured: u64,
    /// The cores of the window before the newest, which ended before any lease the newest
    /// reading has seen uncovered was placed ([`WorkerLoad::measured_before`]); `measured`
    /// when there is no such window, and 0 when charged by prediction alone.
    pub(crate) before: u64,
    /// The working set that counts against memory (0 when charged by prediction alone).
    pub(crate) working_set: u64,
}

impl Left {
    /// The cores a new lease of `cost` is charged on this worker.
    pub(crate) fn charged_cpu(&self, cost: &LeaseCost) -> u64 {
        cost.cpu_millis.max(self.floor_cpu)
    }
}

/// A worker's load state.
#[derive(Clone, Debug, Default)]
pub(crate) struct WorkerLoad {
    readings: Option<Readings>,
    ledger: BTreeMap<LeaseId, Entry>,
    totals: Totals,
    cold: ColdWindow,
    /// An accepted reading has arrived at some time: the worker measures, even if its
    /// signal is gone for now.
    measured_once: bool,
    /// Whether its signal is believed ([`super::plausible`]).
    check: Check,
}

impl WorkerLoad {
    /// Charges `lease` at `cost` from now until [`WorkerLoad::remove`], over time as
    /// `profile` says if it has one. It is uncovered: no reading has listed it.
    pub(crate) fn insert(
        &mut self,
        lease: LeaseId,
        cost: LeaseCost,
        vm: bool,
        profile: Option<Arc<Profile>>,
    ) {
        self.put(lease, Entry::new(cost, vm, profile));
    }

    /// [`WorkerLoad::insert`] for an action lease placement admits at `now`: the
    /// plausibility check can count its CPU time once it ends.
    pub(crate) fn admit(
        &mut self,
        lease: LeaseId,
        cost: LeaseCost,
        profile: Option<Arc<Profile>>,
        now: FarmTime,
    ) {
        let entry = Entry {
            admitted: Some(now),
            ..Entry::new(cost, false, profile)
        };
        self.put(lease, entry);
    }

    fn put(&mut self, lease: LeaseId, entry: Entry) {
        self.totals.shift(&entry, true);
        self.check.lease_present();
        // A lease booked twice is charged once.
        if let Some(old) = self.ledger.insert(lease, entry) {
            self.totals.shift(&old, false);
        }
    }

    /// Takes `lease`'s charge off, whatever its outcome. A lease not charged is ignored.
    pub(crate) fn remove(&mut self, lease: LeaseId) -> Option<Ended> {
        let entry = self.ledger.remove(&lease)?;
        self.totals.shift(&entry, false);
        Some(Ended {
            profile: entry.profile,
            cpu_misfit: entry.cpu_misfit,
            mem_misfit: entry.mem_misfit,
        })
    }

    /// [`WorkerLoad::remove`] of a lease whose run ended at `now`: an action lease placement
    /// admitted is kept for the plausibility check.
    pub(crate) fn end(&mut self, lease: LeaseId, now: FarmTime) -> Option<Ended> {
        if let Some(entry) = self.ledger.get(&lease)
            && !entry.vm
        {
            self.check.ended(lease, entry.admitted, entry.cpu_used, now);
        }
        self.remove(lease)
    }

    pub(crate) const fn totals(&self) -> &Totals {
        &self.totals
    }

    /// What taking each lease off the ledger takes off the left side `left`: the lease, its
    /// cores (at least the floor `left` holds for an action lease) and its memory.
    pub(crate) fn charges(&self, left: &Left) -> Vec<(LeaseId, u64, u64)> {
        self.ledger
            .iter()
            .map(|(lease, entry)| {
                let c = charge(&entry.cost);
                let floor = left.floor_cpu * u64::from(!entry.vm);
                (*lease, c.cpu_millis.max(floor), c.memory_bytes)
            })
            .collect()
    }

    /// The CPU time `lease`'s result reported, `cpu_usec` microseconds, arrived at `now`, for
    /// the plausibility check: kept with the lease until it leaves the ledger, or counted at
    /// once if it has left. Answers whether this worker holds or held `lease`; a VM lease,
    /// whose CPU the check does not count, is not taken.
    pub(crate) fn lease_cpu(&mut self, lease: LeaseId, cpu_usec: u64, now: FarmTime) -> bool {
        match self.ledger.get_mut(&lease) {
            Some(entry) => {
                if !entry.vm {
                    entry.cpu_used = Some(cpu_usec);
                }
                true
            }
            None => self.check.late_cpu(lease, cpu_usec, now),
        }
    }

    /// The worker holds a lease the ledger does not charge (a whole-machine lease): the window
    /// under way is not idle for the plausibility check.
    pub(crate) const fn note_lease(&mut self) {
        self.check.lease_present();
    }

    /// What the plausibility check holds of the signal ([`super::plausible`]).
    pub(crate) const fn check(&self) -> &Check {
        &self.check
    }

    /// A new session: readings of the old one say nothing about the new. The leases
    /// stay charged, uncovered, and what the old session said of their working sets is
    /// forgotten.
    pub(crate) fn new_session(&mut self) {
        self.readings = None;
        self.check.new_session();
        for entry in self.ledger.values_mut() {
            entry.ws_over = 0;
        }
        self.cover(&BTreeSet::new(), false);
    }

    /// Records that one more cold lease was admitted in the window of `now`.
    pub(crate) fn note_cold(&mut self, now: FarmTime, cfg: &LoadConfig) {
        let index = cfg.window_index(now);
        if self.cold.index != index {
            self.cold = ColdWindow { index, admitted: 0 };
        }
        self.cold.admitted += 1;
    }

    /// Cold leases admitted in the window of `now`.
    pub(crate) fn cold_admitted(&self, now: FarmTime, cfg: &LoadConfig) -> u32 {
        if self.cold.index == cfg.window_index(now) {
            self.cold.admitted
        } else {
            0
        }
    }

    /// Rebuilds the sums with `covered` as the leases that ran the whole window; every
    /// action lease on the ledger is `seen` by the reading taken in, or none when there is
    /// no reading (and then `covered` is empty, so every covered lease is seen).
    fn cover(&mut self, covered: &BTreeSet<LeaseId>, seen: bool) {
        self.totals = Totals::default();
        for (lease, entry) in &mut self.ledger {
            entry.covered = !entry.vm && covered.contains(lease);
            entry.seen = !entry.vm && seen;
            entry.aged = !entry.covered || entry.clocked;
            self.totals.shift(entry, true);
        }
    }

    /// Takes the clocks and closed minutes of an accepted reading in: each lease's age,
    /// its newest working set, and the minutes that misfit its curves.
    fn take_clocks(&mut self, load: &NodeLoad, counters: &mut LoadCounters) {
        let clocks: BTreeMap<LeaseId, u64> = load
            .lease_clocks
            .iter()
            .map(|c| (c.lease, c.started_mono_usec))
            .collect();
        for (lease, entry) in &mut self.ledger {
            let started = clocks.get(lease);
            entry.clocked = started.is_some();
            entry.age = started.map_or(0, |s| {
                u32::try_from(load.mono_usec.saturating_sub(*s) / MINUTE_USEC).unwrap_or(u32::MAX)
            });
        }
        for minute in &load.minutes {
            if let Some(entry) = self.ledger.get_mut(&minute.lease) {
                entry.observe(minute, counters);
            }
        }
    }

    /// Takes a heartbeat's reading in, or drops it. `held` is whether the worker holds a
    /// lease of any kind now, the whole-machine leases the ledger does not charge included.
    ///
    /// The first reading of a session is a baseline and gives no window. After that a reading
    /// is accepted when [`NodeLoad::busy_millicores_since`] gives the window (`seq` above the
    /// last accepted one, neither the clock nor the counter back) and the figures are
    /// plausible: no more busy cores than [`IMPLAUSIBLE_FACTOR`] times the worker's, no more
    /// memory than that times its. The cores are judged against `capacity` and the memory
    /// against `raw_memory_bytes`, what the worker reported: a working set is measured on the
    /// whole machine, so the share admission books does not make a legitimate reading
    /// implausible. The window kept is the cores demanded. A dropped reading forgets the
    /// baseline too, so the worker is charged by prediction until two in a row are accepted.
    /// Each accepted reading is handed to the plausibility check ([`super::plausible`]),
    /// which can mark the signal, and then the worker is charged by prediction too.
    pub(crate) fn accept(
        &mut self,
        load: NodeLoad,
        now: FarmTime,
        (capacity, raw_memory_bytes, held): (&Resources, u64, bool),
        cfg: &LoadConfig,
        counters: &mut LoadCounters,
    ) {
        let window = match &self.readings {
            None => None,
            Some(r) => match load.busy_millicores_since(&r.last) {
                Some(cores) => Some((cores, load.demand_millicores_since(&r.last, cores))),
                None => return self.drop_readings(counters),
            },
        };
        let too_many_cores = window.is_some_and(|(cores, _)| {
            capacity.cpu_millis > 0
                && u64::from(cores) > capacity.cpu_millis.saturating_mul(IMPLAUSIBLE_FACTOR)
        });
        let too_much_memory = raw_memory_bytes > 0
            && load.memory_working_set_bytes > raw_memory_bytes.saturating_mul(IMPLAUSIBLE_FACTOR);
        if too_many_cores || too_much_memory {
            return self.drop_readings(counters);
        }
        counters.readings_accepted += 1;
        self.measured_once = true;
        let mark = Mark::new(now, load.mono_usec, load.cpu_usage_usec);
        match window {
            None => self.check.baseline(mark, held),
            Some((cores, _)) => {
                self.check
                    .window(mark, (cores, capacity.cpu_millis), held, counters);
            }
        }
        let older = self.readings.as_ref().and_then(|r| r.windows[0]);
        let covered: BTreeSet<LeaseId> = load.covered.iter().copied().collect();
        self.take_clocks(&load, counters);
        self.readings = Some(Readings {
            last: load,
            arrived: now,
            windows: [window.map(|(_, demand)| demand), older],
        });
        self.cover(&covered, true);
        if let Some(measured) = self.measured() {
            counters.floor_windows += 1;
            if self.totals.cpu_covered_at(0, cfg) > u64::from(measured) {
                counters.floor_binds += 1;
            }
        }
    }

    fn drop_readings(&mut self, counters: &mut LoadCounters) {
        counters.readings_dropped += 1;
        self.new_session();
    }

    /// Cores demanded, busy and waiting, in millicores: the larger of the last two windows,
    /// so a rise is believed at once and a fall after two windows. `None` before the first.
    pub(crate) fn measured(&self) -> Option<u32> {
        let r = self.readings.as_ref()?;
        r.windows.iter().flatten().copied().max()
    }

    /// Cores demanded, in millicores, over the window before the newest, which ended at the
    /// reading before the newest, so it holds none of the CPU of a lease placed after it.
    /// [`WorkerLoad::measured`] when there is no such window yet.
    pub(crate) fn measured_before(&self) -> Option<u32> {
        let r = self.readings.as_ref()?;
        r.windows[1].or_else(|| self.measured())
    }

    /// Whether the measured CPU, the whole host's, is above the covered leases' predicted sum
    /// (`M > P_c`, before the floor): the predictions are too low, or other work loads the host.
    pub(crate) fn measured_above_covered(&self) -> bool {
        self.measured()
            .is_some_and(|m| u64::from(m) > self.totals.cpu_covered)
    }

    /// The predicted cores of the action leases, covered or not, whose key hash is
    /// `hash`. A hash of 0 (no key known) matches none.
    pub(crate) fn same_key_millis(&self, hash: u64) -> u64 {
        if hash == 0 {
            return 0;
        }
        self.ledger
            .values()
            .filter(|e| !e.vm && e.cost.key_hash == hash)
            .fold(0, |sum, e| sum.saturating_add(charge(&e.cost).cpu_millis))
    }

    /// How many action leases no reading has covered yet.
    pub(crate) fn uncovered_leases(&self) -> u32 {
        let n = self.ledger.values().filter(|e| !e.vm && !e.covered).count();
        u32::try_from(n).unwrap_or(u32::MAX)
    }

    /// The newest reading when the worker is charged by measurement: it has a window, the
    /// reading is not older than [`js_types::sched::usage::STALE_WINDOWS`] windows at `now`,
    /// the actions cgroup has no CPU limit and the plausibility check has not marked the
    /// signal.
    fn fresh_reading(&self, now: FarmTime, cfg: &LoadConfig) -> Option<&NodeLoad> {
        let r = self.readings.as_ref()?;
        if r.windows[0].is_none() || r.last.actions_cpu_limited || self.check.implausible() {
            return None;
        }
        // The age is read on the scheduler's clock and carried over to the agent's, so
        // the one staleness rule is `NodeLoad::is_stale`.
        let age =
            u64::try_from(now.saturating_duration_since(r.arrived).as_micros()).unwrap_or(u64::MAX);
        let stale = r
            .last
            .is_stale(cfg.window_usec(), r.last.mono_usec.saturating_add(age));
        (!stale).then_some(&r.last)
    }

    /// Whether the worker is charged by measurement ([`WorkerLoad::fresh_reading`]).
    pub(crate) fn fresh(&self, now: FarmTime, cfg: &LoadConfig) -> bool {
        self.fresh_reading(now, cfg).is_some()
    }

    /// Whether the newest reading says the actions cgroup has a CPU limit.
    pub(crate) fn cpu_limited(&self) -> bool {
        self.readings
            .as_ref()
            .is_some_and(|r| r.last.actions_cpu_limited)
    }

    /// The newest reading's working set, if there is one.
    pub(crate) fn working_set(&self) -> Option<u64> {
        self.readings
            .as_ref()
            .map(|r| r.last.memory_working_set_bytes)
    }

    /// The working set that counts against memory, of the fresh `reading`.
    ///
    /// `memory.current` minus the inactive file pages still holds the hot (active) file
    /// pages of leases that have ended, and the kernel reclaims them only under pressure,
    /// which a node that admits nothing never feels. Counted whole they could fill the
    /// node's memory on an idle node and refuse every lease for good. So the active file
    /// pages count for at most [`LoadConfig::file_share_permille`] of the node's memory
    /// ([`NodeLoad::admission_working_set`]: anon and kernel memory in full plus a bounded
    /// share of the file pages), and a node with no action lease counts none of the
    /// working set: it is all residue, and an idle node admits anything that fits it.
    fn working_set_counted(
        &self,
        reading: &NodeLoad,
        capacity: &Resources,
        cfg: &LoadConfig,
    ) -> u64 {
        if self.totals.actions == 0 {
            return 0;
        }
        reading.admission_working_set(capacity.memory_bytes, cfg.file_share_permille)
    }

    /// The left side of the predicate: what the worker is charged before the new lease.
    ///
    /// By measurement (a fresh signal):
    ///
    /// ```text
    /// cpu = max(measured, max(before, f * sum p(covered)) + sum p(seen)) + sum p(unseen) + V_cpu
    /// mem = max(sum m(all), working_set + sum m(uncovered)) + V_mem
    /// ```
    ///
    /// A lease placed before the newest reading was taken in is seen by it: some of what it
    /// has used is in the measurement, so its cores are added to the window before the
    /// newest ([`WorkerLoad::measured_before`]), not to the measurement. Only the leases
    /// placed after that reading (unseen) are charged on top of the measurement.
    ///
    /// Without one, the fallback charges every lease its full predicted cost once, and
    /// the working set is not read. On a worker that never sent an accepted reading each
    /// action lease is charged at least [`UNMEASURED_CPU_FLOOR_MILLIS`]; a worker that has
    /// measured and gone stale is charged the estimates as they are.
    pub(crate) fn left(&self, now: FarmTime, capacity: &Resources, cfg: &LoadConfig) -> Left {
        let t = &self.totals;
        if let Some(reading) = self.fresh_reading(now, cfg) {
            let measured = u64::from(self.measured().unwrap_or(0));
            let before = u64::from(self.measured_before().unwrap_or(0));
            let ws = self.working_set_counted(reading, capacity, cfg);
            Left {
                cpu: horizon::cpu_left(t, 0, (measured, before), cfg),
                mem: horizon::mem_left(t, 0, ws),
                fallback: false,
                floor_cpu: 0,
                measured,
                before,
                working_set: ws,
            }
        } else if self.measured_once {
            Left {
                cpu: t.cpu_all().saturating_add(t.vm_cpu),
                mem: t.mem_all().saturating_add(t.vm_mem),
                fallback: true,
                floor_cpu: 0,
                measured: 0,
                before: 0,
                working_set: 0,
            }
        } else {
            let floor = UNMEASURED_CPU_FLOOR_MILLIS.min(capacity.cpu_millis);
            let floored = self
                .ledger
                .values()
                .filter(|e| !e.vm)
                .map(|e| charge(&e.cost).cpu_millis.max(floor))
                .fold(0, u64::saturating_add);
            Left {
                cpu: floored.saturating_add(t.vm_cpu),
                mem: t.mem_all().saturating_add(t.vm_mem),
                fallback: true,
                floor_cpu: floor,
                measured: 0,
                before: 0,
                working_set: 0,
            }
        }
    }

    /// What the worker is charged `slot` minutes ahead (slot [`super::profile::H_PLUS`]: from
    /// the horizon on), as `(cores in millicores, memory in bytes)`, before a new lease.
    /// A worker charged by prediction alone is charged the same at every slot.
    pub(crate) fn left_at(
        &self,
        slot: usize,
        now: FarmTime,
        capacity: &Resources,
        cfg: &LoadConfig,
    ) -> (u64, u64) {
        let left = self.left(now, capacity, cfg);
        if left.fallback {
            return (left.cpu, left.mem);
        }
        let t = &self.totals;
        (
            horizon::cpu_left(t, slot, (left.measured, left.before), cfg),
            horizon::mem_left(t, slot, left.working_set),
        )
    }

    /// The flat-memory guard for a lease of `cost` on this worker
    /// ([`horizon::flat_memory_refusal`]); a VM, which has no predicate, is held to it
    /// alone, with its booked memory as its cost.
    pub(crate) fn flat_memory_refusal(
        &self,
        cost: &LeaseCost,
        capacity: &Resources,
        cfg: &LoadConfig,
    ) -> Option<Refusal> {
        horizon::flat_memory_refusal(&self.totals, cost, capacity, cfg)
    }

    /// Whether a lease of `cost` (already clamped to the capacity) fits beside `left`.
    ///
    /// On a worker charged by measurement, when the new lease or a lease on the worker is
    /// charged over time, the lease must fit at every slot of the horizon
    /// ([`horizon::refusal`]); otherwise at the one flat figure. `profile` is how the new
    /// lease is charged over time, if it is. With `cpu_judged` ([`crate::Scheduler::set_cpu_judged`])
    /// no CPU inequality is tested, at one figure or at any slot. Then the flat-memory guard
    /// ([`horizon::flat_memory_refusal`]), which every lease that adds memory passes on
    /// either path. Only actions come here: a whole-machine lease is not judged, and an
    /// action at the whole host's memory is placed on a worker with no lease, whose flat
    /// sum is empty, and its cost is clamped to the worker's memory.
    pub(crate) fn admits(
        &self,
        cost: &LeaseCost,
        profile: Option<&Profile>,
        left: &Left,
        capacity: &Resources,
        (now, cpu_judged): (FarmTime, bool),
        cfg: &LoadConfig,
    ) -> Result<(), Refusal> {
        if !left.fallback && (self.totals.has_timed() || profile.is_some_and(Profile::timed)) {
            let over =
                horizon::refusal(&self.totals, cost, profile, left, capacity, cfg, cpu_judged);
            over.map_or(Ok(()), Err)?;
        } else {
            let (new, limit) = (left.charged_cpu(cost), cfg.cpu_limit(capacity));
            if !cpu_judged && left.cpu.saturating_add(new) > limit {
                let refusal = Refusal::new(RefusalKind::Cpu, left.cpu, new, limit);
                return Err(refusal.measured((!left.fallback).then_some(left.measured)));
            }
            let limit = cfg.mem_limit(capacity);
            if left.mem.saturating_add(cost.memory_bytes) > limit {
                return Err(Refusal::new(
                    RefusalKind::PredictedMemory,
                    left.mem,
                    cost.memory_bytes,
                    limit,
                ));
            }
        }
        if let Some(refusal) = self.flat_memory_refusal(cost, capacity, cfg) {
            return Err(refusal);
        }
        let admitted = self.cold_admitted(now, cfg);
        let cap = cfg.cold_cap(capacity);
        if cost.cold && admitted >= cap {
            return Err(Refusal::new(
                RefusalKind::ColdCap,
                u64::from(admitted),
                1,
                u64::from(cap),
            ));
        }
        Ok(())
    }
}

/// `cost` as charged on a worker of `capacity`: never more than the worker has, so a
/// key that wants the whole node runs on an otherwise idle one.
pub(crate) fn clamped(cost: LeaseCost, capacity: &Resources) -> LeaseCost {
    LeaseCost {
        cpu_millis: cost.cpu_millis.min(capacity.cpu_millis),
        memory_bytes: cost.memory_bytes.min(capacity.memory_bytes),
        ..cost
    }
}

#[cfg(test)]
#[path = "ledger_tests.rs"]
mod tests;
