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
use super::profile::{Lanes, Profile};
use super::{LoadConfig, LoadCounters, charge};

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
}

impl Entry {
    fn new(cost: LeaseCost, vm: bool, profile: Option<Arc<Profile>>) -> Self {
        Self {
            cost,
            vm,
            covered: false,
            profile,
            age: 0,
            clocked: false,
            aged: true,
            ws_over: 0,
            seen_minute: None,
            cpu_misfit: false,
            mem_misfit: false,
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
    /// Cores busy over the last two windows, newest first, in millicores. A window is
    /// known once two consecutive readings have been accepted.
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
    /// The measured cores, in millicores, the left side holds (0 when it is charged by
    /// prediction alone).
    pub(crate) measured: u64,
    /// The working set that counts against memory (0 when charged by prediction alone).
    pub(crate) working_set: u64,
}

impl Left {
    /// The cores a new lease of `cost` is charged on this worker.
    pub(crate) fn charged_cpu(&self, cost: &LeaseCost) -> u64 {
        cost.cpu_millis.max(self.floor_cpu)
    }
}

/// Why the predicate refused a lease.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    Cpu,
    Memory,
    /// Only the cold cap of the window stands in the way.
    ColdCap,
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
        let entry = Entry::new(cost, vm, profile);
        self.totals.shift(&entry, true);
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

    pub(crate) const fn totals(&self) -> &Totals {
        &self.totals
    }

    /// A new session: readings of the old one say nothing about the new. The leases
    /// stay charged, uncovered, and what the old session said of their working sets is
    /// forgotten.
    pub(crate) fn new_session(&mut self) {
        self.readings = None;
        for entry in self.ledger.values_mut() {
            entry.ws_over = 0;
        }
        self.cover(&BTreeSet::new());
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

    fn cover(&mut self, covered: &BTreeSet<LeaseId>) {
        self.totals = Totals::default();
        for (lease, entry) in &mut self.ledger {
            entry.covered = !entry.vm && covered.contains(lease);
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

    /// Takes a heartbeat's reading in, or drops it.
    ///
    /// The first reading of a session is a baseline and gives no window. After that a
    /// reading is accepted when [`NodeLoad::busy_millicores_since`] gives the window
    /// (its `seq` is above the last accepted one and neither the clock nor the counter
    /// went back) and the figures are plausible: no more cores than
    /// [`IMPLAUSIBLE_FACTOR`] times the worker's, no more memory than that times its.
    /// The cores are judged against `capacity` and the memory against `raw_memory_bytes`,
    /// what the worker reported: a working set is measured on the whole machine, so the
    /// share admission books does not make a legitimate reading implausible.
    /// A dropped reading forgets the baseline too, so the worker is charged by
    /// prediction until two readings in a row are accepted again.
    pub(crate) fn accept(
        &mut self,
        load: NodeLoad,
        now: FarmTime,
        capacity: &Resources,
        raw_memory_bytes: u64,
        cfg: &LoadConfig,
        counters: &mut LoadCounters,
    ) {
        let window = match &self.readings {
            None => None,
            Some(r) => match load.busy_millicores_since(&r.last) {
                Some(cores) => Some(cores),
                None => return self.drop_readings(counters),
            },
        };
        let too_many_cores = window.is_some_and(|cores| {
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
        let older = self.readings.as_ref().and_then(|r| r.windows[0]);
        let covered: BTreeSet<LeaseId> = load.covered.iter().copied().collect();
        self.take_clocks(&load, counters);
        self.readings = Some(Readings {
            last: load,
            arrived: now,
            windows: [window, older],
        });
        self.cover(&covered);
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

    /// Cores busy, in millicores: the larger of the last two windows, so a rise is
    /// believed at once and a fall after two windows. `None` before the first window.
    pub(crate) fn measured(&self) -> Option<u32> {
        let r = self.readings.as_ref()?;
        r.windows.iter().flatten().copied().max()
    }

    /// Whether the measured CPU is above the predicted sum of the covered leases
    /// (`M > P_c`): the predictions are known to be too low here. Plain, before the floor.
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
    /// reading is not older than [`js_types::sched::usage::STALE_WINDOWS`] windows at `now`
    /// and the actions cgroup has no CPU limit.
    fn fresh_reading(&self, now: FarmTime, cfg: &LoadConfig) -> Option<&NodeLoad> {
        let r = self.readings.as_ref()?;
        if r.windows[0].is_none() || r.last.actions_cpu_limited {
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
    /// cpu = max(measured, f * sum p(covered)) + sum p(uncovered) + V_cpu
    /// mem = max(sum m(all), working_set + sum m(uncovered)) + V_mem
    /// ```
    ///
    /// Without one, the fallback charges every lease its full predicted cost once, and
    /// the working set is not read. On a worker that never sent an accepted reading each
    /// action lease is charged at least [`UNMEASURED_CPU_FLOOR_MILLIS`]; a worker that has
    /// measured and gone stale is charged the estimates as they are.
    pub(crate) fn left(&self, now: FarmTime, capacity: &Resources, cfg: &LoadConfig) -> Left {
        let t = &self.totals;
        if let Some(reading) = self.fresh_reading(now, cfg) {
            let measured = u64::from(self.measured().unwrap_or(0));
            let ws = self.working_set_counted(reading, capacity, cfg);
            Left {
                cpu: horizon::cpu_left(t, 0, measured, cfg),
                mem: horizon::mem_left(t, 0, ws),
                fallback: false,
                floor_cpu: 0,
                measured,
                working_set: ws,
            }
        } else if self.measured_once {
            Left {
                cpu: t.cpu_all().saturating_add(t.vm_cpu),
                mem: t.mem_all().saturating_add(t.vm_mem),
                fallback: true,
                floor_cpu: 0,
                measured: 0,
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
            horizon::cpu_left(t, slot, left.measured, cfg),
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
    /// lease is charged over time, if it is. Then the flat-memory guard
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
        now: FarmTime,
        cfg: &LoadConfig,
    ) -> Result<(), Refusal> {
        if !left.fallback && (self.totals.has_timed() || profile.is_some_and(Profile::timed)) {
            if let Some(refusal) =
                horizon::refusal(&self.totals, cost, profile, left, capacity, cfg)
            {
                return Err(refusal);
            }
        } else {
            if left.cpu.saturating_add(left.charged_cpu(cost)) > cfg.cpu_limit(capacity) {
                return Err(Refusal::Cpu);
            }
            if left.mem.saturating_add(cost.memory_bytes) > cfg.mem_limit(capacity) {
                return Err(Refusal::Memory);
            }
        }
        if let Some(refusal) = self.flat_memory_refusal(cost, capacity, cfg) {
            return Err(refusal);
        }
        if cost.cold && self.cold_admitted(now, cfg) >= cfg.cold_cap(capacity) {
            return Err(Refusal::ColdCap);
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
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    fn cost(cpu: u64, mem: u64) -> LeaseCost {
        LeaseCost {
            cpu_millis: cpu,
            memory_bytes: mem,
            limit_bytes: mem,
            cold: false,
            retry: false,
            key_hash: 0,
        }
    }

    fn lease(n: u64) -> LeaseId {
        LeaseId::new(1, n)
    }

    /// Catches: a lease booked twice charged twice (a recovered lease adopted again would
    /// double the worker's sums for good), and removing a lease that is not charged
    /// changing the sums.
    #[test]
    fn a_lease_is_charged_once_and_removing_a_stranger_changes_nothing() {
        let mut load = WorkerLoad::default();
        load.insert(lease(1), cost(500, GIB), false, None);
        load.insert(lease(1), cost(700, 2 * GIB), false, None);
        assert_eq!(load.totals().cpu_uncovered, 700);
        assert_eq!(load.totals().mem_uncovered, 2 * GIB);
        assert_eq!(load.totals().actions, 1);
        load.remove(lease(9));
        assert_eq!(load.totals().cpu_uncovered, 700);
        load.remove(lease(1));
        assert_eq!(*load.totals(), Totals::default());
    }

    fn curve(points: &[(u32, u64)]) -> js_types::sched::usage::Curve {
        let points = points
            .iter()
            .map(
                |&(cores_milli, working_set_bytes)| js_types::sched::usage::MinutePoint {
                    cores_milli,
                    working_set_bytes,
                },
            )
            .collect();
        js_types::sched::usage::Curve::new(points, 5).expect("a short curve")
    }

    /// A profile of a lease of `cost(20_000, 2 GiB)`: the curve of cores 10 then 60 and
    /// working set 4 GiB then 1 GiB, by curve in CPU and in memory as asked, or none and
    /// a life of three minutes.
    fn profile(by_curve: bool, cpu: bool, mem: bool, life: Option<u32>) -> Arc<Profile> {
        use super::super::profile::Choice;
        Arc::new(Profile::new(
            Choice {
                curve: by_curve.then(|| curve(&[(10_000, 4 * GIB), (60_000, GIB)])),
                cpu,
                mem,
                demoted_cpu: false,
                demoted_mem: false,
                life,
                floor_permille: 750,
                guard_key: 1,
            },
            &cost(20_000, 2 * GIB),
        ))
    }

    /// Catches: the per-slot lanes missing a kind of lease, or counting a lease in the
    /// wrong lane or twice, or not giving it back: six leases (charged by curve in both
    /// resources, covered and not; flat to a life; by curve in CPU only; by curve in
    /// memory only; covered with no clock, which is flat) are summed slot by slot and taken
    /// off to nothing.
    #[test]
    fn the_lanes_sum_each_kind_of_lease_by_slot_and_empty_when_they_are_removed() {
        use js_types::sched::usage::LeaseClock;
        let mut load = WorkerLoad::default();
        let kinds = [
            (profile(true, true, true, None), false),
            (profile(true, true, true, None), true),
            (profile(false, true, true, Some(3)), true),
            (profile(true, true, false, None), false),
            (profile(true, true, true, None), true),
            (profile(true, false, true, None), false),
        ];
        for (i, (p, _)) in kinds.iter().enumerate() {
            load.insert(
                lease(i as u64),
                cost(20_000, 2 * GIB),
                false,
                Some(p.clone()),
            );
        }
        // A floor below 1, so that the sum of the flat covered leases shows the floor.
        let cfg = LoadConfig {
            cpu_floor_permille: 750,
            ..LoadConfig::default()
        };
        let capacity = Resources::new(100_000, 64 * GIB);
        let mut counters = LoadCounters::default();
        let read = |seq: u64, covered: Vec<LeaseId>, clocks: Vec<LeaseClock>| NodeLoad {
            seq,
            mono_usec: 3_600_000_000 + seq * 5_000_000,
            covered,
            lease_clocks: clocks,
            ..NodeLoad::default()
        };
        let now = FarmTime::from_millis(0);
        load.accept(
            read(1, Vec::new(), Vec::new()),
            now,
            &capacity,
            capacity.memory_bytes,
            &cfg,
            &mut counters,
        );
        // Leases 1, 2 and 4 are covered; 1 is a minute old, 2 is new, 4 has no clock.
        let second = read(2, vec![lease(1), lease(2), lease(4)], Vec::new());
        let clock = |l: u64, age_minutes: u64| LeaseClock {
            lease: lease(l),
            started_mono_usec: second.mono_usec - age_minutes * 60_000_000,
        };
        let second = NodeLoad {
            lease_clocks: vec![clock(1, 1), clock(2, 0)],
            ..second
        };
        load.accept(
            second,
            now,
            &capacity,
            capacity.memory_bytes,
            &cfg,
            &mut counters,
        );
        let t = load.totals();
        assert!(t.has_timed());
        // Covered by curve (lease 1: 60 cores from its minute 1); covered flat to a life
        // (lease 2: 20 cores for three minutes, a floor of 0.75 on the sum); the flat
        // lease 4 stays in the scalar sum (20 cores).
        assert_eq!(
            t.cpu_covered_at(0, &cfg),
            60_000 + (20_000 + 20_000) * 3 / 4
        );
        assert_eq!(t.cpu_covered_at(5, &cfg), 60_000 + 20_000 * 3 / 4);
        // Uncovered: leases 0 and 3 by curve (60 cores, the larger of minutes 0 and 1 and
        // then the tail), lease 5 flat in CPU (20).
        assert_eq!(t.cpu_uncovered_at(0), 60_000 + 60_000 + 20_000);
        assert_eq!(t.cpu_uncovered_at(31), 60_000 + 60_000 + 20_000);
        // Memory: lease 0 and 5 hold 4 GiB then 1; lease 1 holds 1 (from its minute 1);
        // lease 2 holds 2 for three minutes; leases 3 and 4 flat (2 each).
        assert_eq!(t.mem_all_at(0), (4 + 1 + 2 + 2 + 2 + 4) * GIB);
        assert_eq!(t.mem_all_at(1), (1 + 1 + 2 + 2 + 2 + 1) * GIB);
        // Lease 2 has ended its life by minute 3.
        assert_eq!(t.mem_all_at(3), (1 + 1 + 2 + 2 + 1) * GIB);
        assert_eq!(t.mem_uncovered_at(0), (4 + 2 + 4) * GIB);
        assert_eq!(t.mem_uncovered_at(1), (1 + 2 + 1) * GIB);
        for i in 0..6 {
            load.remove(lease(i));
        }
        assert_eq!(*load.totals(), Totals::default());
    }

    fn keyed(cpu: u64, hash: u64) -> LeaseCost {
        LeaseCost {
            key_hash: hash,
            ..cost(cpu, 0)
        }
    }

    /// Catches: the same-key sum counting a lease of another key, a VM, or leases with no
    /// key known (hash 0 matches nothing, not each other), or not counting covered and
    /// uncovered leases alike.
    #[test]
    fn the_same_key_sum_counts_only_that_key_covered_or_not() {
        let mut load = WorkerLoad::default();
        load.insert(lease(1), keyed(1_000, 7), false, None);
        load.insert(lease(2), keyed(2_000, 7), false, None);
        load.insert(lease(3), keyed(4_000, 8), false, None);
        load.insert(lease(4), keyed(8_000, 7), true, None);
        load.insert(lease(5), keyed(16_000, 0), false, None);
        load.cover(&BTreeSet::from([lease(1)]));
        assert_eq!(load.same_key_millis(7), 3_000);
        assert_eq!(load.same_key_millis(8), 4_000);
        assert_eq!(load.same_key_millis(9), 0);
        assert_eq!(load.same_key_millis(0), 0);
    }

    /// Catches: the uncovered count taking covered leases or VMs in, or not following a
    /// reading that covers a lease.
    #[test]
    fn uncovered_leases_are_the_actions_no_reading_lists() {
        let mut load = WorkerLoad::default();
        load.insert(lease(1), cost(1_000, 0), false, None);
        load.insert(lease(2), cost(1_000, 0), false, None);
        load.insert(lease(3), cost(1_000, 0), true, None);
        assert_eq!(load.uncovered_leases(), 2);
        load.cover(&BTreeSet::from([lease(2), lease(3)]));
        assert_eq!(load.uncovered_leases(), 1);
    }

    /// Catches: `M > P_c` compared with the floor or with all leases instead of the covered
    /// ones, and true before any window: the measurement must be strictly above the sum of
    /// the covered leases' predictions.
    #[test]
    fn measured_above_predicted_compares_with_the_covered_sum_only() {
        let readings = |windows: [Option<u32>; 2]| Readings {
            last: NodeLoad::default(),
            arrived: FarmTime::from_millis(0),
            windows,
        };
        let mut load = WorkerLoad::default();
        load.insert(lease(1), cost(1_000, 0), false, None);
        load.insert(lease(2), cost(5_000, 0), false, None);
        load.cover(&BTreeSet::from([lease(1)]));
        assert!(!load.measured_above_covered(), "no window yet");
        load.readings = Some(readings([Some(1_000), None]));
        assert!(!load.measured_above_covered(), "equal is not above");
        load.readings = Some(readings([Some(1_001), None]));
        assert!(load.measured_above_covered());
        load.readings = Some(readings([Some(900), Some(1_001)]));
        assert!(load.measured_above_covered(), "the larger of two windows");
    }
}
