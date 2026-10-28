//! One worker's load as admission sees it: the readings it sent, the cost fixed for each
//! lease placed on it, and the sums the predicate reads.
//!
//! Everything here is integer arithmetic on plain values; time is the scheduler's
//! [`FarmTime`], never a clock.

use std::collections::{BTreeMap, BTreeSet};

use js_types::sched::usage::{LeaseCost, NodeLoad};
use js_types::sched::{FarmTime, LeaseId, Resources};

use super::{LoadConfig, LoadCounters, charge};

/// A reading is dropped when it implies more cores than this many times the worker's
/// capacity, or more memory than this many times its capacity.
const IMPLAUSIBLE_FACTOR: u64 = 4;

/// The least a lease is charged on a worker that has never sent an accepted reading (a
/// Mac, a native or VM node): nothing there measures a light population, so only the
/// predictions protect it. A worker that has measured keeps the sub-core charge of the
/// estimate (250 m at least) even when its signal has gone stale.
pub(crate) const UNMEASURED_CPU_FLOOR_MILLIS: u64 = 1000;

/// One lease on the ledger.
#[derive(Clone, Copy, Debug)]
struct Entry {
    cost: LeaseCost,
    /// A VM lease: it runs outside the actions cgroup, so its booked cost is added to
    /// every sum and no reading covers it.
    vm: bool,
    /// The newest accepted reading lists it as running for the whole window.
    covered: bool,
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
}

impl Totals {
    fn shift(&mut self, entry: &Entry, up: bool) {
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
    /// Charges `lease` at `cost` from now until [`WorkerLoad::remove`]. It is uncovered:
    /// no reading has listed it.
    pub(crate) fn insert(&mut self, lease: LeaseId, cost: LeaseCost, vm: bool) {
        let entry = Entry {
            cost,
            vm,
            covered: false,
        };
        // A lease booked twice is charged once.
        if let Some(old) = self.ledger.insert(lease, entry) {
            self.totals.shift(&old, false);
        }
        self.totals.shift(&entry, true);
    }

    /// Takes `lease`'s charge off, whatever its outcome. A lease not charged is ignored.
    pub(crate) fn remove(&mut self, lease: LeaseId) {
        if let Some(entry) = self.ledger.remove(&lease) {
            self.totals.shift(&entry, false);
        }
    }

    pub(crate) const fn totals(&self) -> &Totals {
        &self.totals
    }

    /// A new session: readings of the old one say nothing about the new. The leases
    /// stay charged.
    pub(crate) fn new_session(&mut self) {
        self.readings = None;
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
            self.totals.shift(entry, true);
        }
    }

    /// Takes a heartbeat's reading in, or drops it.
    ///
    /// The first reading of a session is a baseline and gives no window. After that a
    /// reading is accepted when [`NodeLoad::busy_millicores_since`] gives the window
    /// (its `seq` is above the last accepted one and neither the clock nor the counter
    /// went back) and the figures are plausible: no more cores than
    /// [`IMPLAUSIBLE_FACTOR`] times the worker's, no more memory than that times its.
    /// A dropped reading forgets the baseline too, so the worker is charged by
    /// prediction until two readings in a row are accepted again.
    pub(crate) fn accept(
        &mut self,
        load: NodeLoad,
        now: FarmTime,
        capacity: &Resources,
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
        let too_much_memory = capacity.memory_bytes > 0
            && load.memory_working_set_bytes
                > capacity.memory_bytes.saturating_mul(IMPLAUSIBLE_FACTOR);
        if too_many_cores || too_much_memory {
            return self.drop_readings(counters);
        }
        counters.readings_accepted += 1;
        self.measured_once = true;
        let older = self.readings.as_ref().and_then(|r| r.windows[0]);
        let covered: BTreeSet<LeaseId> = load.covered.iter().copied().collect();
        self.readings = Some(Readings {
            last: load,
            arrived: now,
            windows: [window, older],
        });
        self.cover(&covered);
        if let Some(measured) = self.measured() {
            counters.floor_windows += 1;
            if cfg.floor_of(self.totals.cpu_covered) > u64::from(measured) {
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
            let cpu = measured.max(cfg.floor_of(t.cpu_covered));
            let ws = self.working_set_counted(reading, capacity, cfg);
            Left {
                cpu: cpu.saturating_add(t.cpu_uncovered).saturating_add(t.vm_cpu),
                mem: t
                    .mem_all()
                    .max(ws.saturating_add(t.mem_uncovered))
                    .saturating_add(t.vm_mem),
                fallback: false,
                floor_cpu: 0,
            }
        } else if self.measured_once {
            Left {
                cpu: t.cpu_all().saturating_add(t.vm_cpu),
                mem: t.mem_all().saturating_add(t.vm_mem),
                fallback: true,
                floor_cpu: 0,
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
            }
        }
    }

    /// Whether a lease of `cost` (already clamped to the capacity) fits beside `left`.
    pub(crate) fn admits(
        &self,
        cost: &LeaseCost,
        left: &Left,
        capacity: &Resources,
        now: FarmTime,
        cfg: &LoadConfig,
    ) -> Result<(), Refusal> {
        if left.cpu.saturating_add(left.charged_cpu(cost)) > cfg.cpu_limit(capacity) {
            return Err(Refusal::Cpu);
        }
        if left.mem.saturating_add(cost.memory_bytes) > cfg.mem_limit(capacity) {
            return Err(Refusal::Memory);
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
        load.insert(lease(1), cost(500, GIB), false);
        load.insert(lease(1), cost(700, 2 * GIB), false);
        assert_eq!(load.totals().cpu_uncovered, 700);
        assert_eq!(load.totals().mem_uncovered, 2 * GIB);
        assert_eq!(load.totals().actions, 1);
        load.remove(lease(9));
        assert_eq!(load.totals().cpu_uncovered, 700);
        load.remove(lease(1));
        assert_eq!(*load.totals(), Totals::default());
    }
}
