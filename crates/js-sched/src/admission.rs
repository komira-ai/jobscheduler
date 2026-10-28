//! Admission by measured and predicted load: the flat rule of the placement-by-load
//! design.
//!
//! An action is admitted to a worker when the worker's load, plus the predicted cost of
//! what the measurement does not yet cover, plus the predicted cost of the new lease,
//! stays within the worker's capacity, for CPU and for memory separately. With `C` the
//! worker's capacity, `p` the cores and `m` the memory peak a lease was charged:
//!
//! ```text
//! CPU:    max(measured, f * sum p(covered)) + sum p(uncovered) + V_cpu + p_new  <=  C_cpu * (1 + s_cpu)
//! memory: max(sum m(all), working_set + sum m(uncovered)) + V_mem + m_new       <=  C_mem * (1 + s_mem)
//! ```
//!
//! - `measured` is the cores busy over the last two windows of the worker's readings, the
//!   larger, so a rise counts at once and a fall after two windows.
//! - A lease is *covered* once a reading lists it as running for the whole window since
//!   the previous reading; until then it is *uncovered* and carries its full cost.
//! - Predicted memory is held to the lease's end and never decays.
//! - `f` ([`LoadConfig::cpu_floor_permille`]) keeps a covered lease in an idle phase
//!   from handing its room back to the next batch.
//! - `V` is the booked cost of the worker's VM leases, which run outside the measured
//!   cgroup.
//! - A worker with no usable measurement is admitted by the same predicate on the sum of
//!   its leases' predicted costs ([`Left::fallback`]); never by the booked rule. Each lease
//!   on a worker that never sent an accepted reading (a Mac, a native or VM node) is
//!   charged at least 1000 m there; a worker that measured and went stale is not.
//! - At most [`LoadConfig::cold_cap`] cold leases are admitted per worker per window.
//! - A blocked head of the queue holds the worker with the most headroom, so a stream
//!   of small work cannot keep a large action out for good (`head.rs`).
//!
//! The cost of a lease ([`LeaseCost`]) is fixed when it is placed, written into its
//! grant record, and kept by the scheduler and by any scheduler recovered from the
//! record until the lease's result; it is never computed again from the estimator.
//!
//! Memory, three ways: the lease's predicted peak (`m`, what is summed), the limit it
//! runs under (the rung of the memory ladder, in [`LeaseCost::limit_bytes`]) and the
//! request's `resources.memory_bytes` (the limit it first runs under) are different
//! figures and are kept apart.
//!
//! Where a lease's charge is computed there is one function, [`charge`]; the rule that
//! charges a lease by a curve over its lifetime replaces it.

mod head;
mod ledger;

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use js_sizing::{COLD_CPU_MILLIS, Estimate};
use js_types::sched::usage::{CurveSource, LeaseCost, NoCurves, key_hash};
use js_types::sched::{FarmTime, LeaseKind, OperationId, Resources, WorkKey};

pub(crate) use head::{Ratio, binding, pick};
pub(crate) use ledger::{Left, Refusal, WorkerLoad, clamped};

use crate::input::Request;

/// Whether placement reads the load.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PlacementLoad {
    /// Admit and rank by measured and predicted load.
    #[default]
    On,
    /// Place as `Off` does, and count what `On` would have decided
    /// ([`LoadCounters::would_admit`], [`LoadCounters::would_block`]).
    Shadow,
    /// Admit by the booked rule: free booked CPUs, memory never summed.
    Off,
}

/// The settings of the load predicate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoadConfig {
    /// Whether placement reads the load. Default `On`.
    pub mode: PlacementLoad,
    /// The heartbeat interval `W`; readings are stale after three of them and the cold
    /// cap counts per window. Default 5 s.
    pub window: Duration,
    /// `f`, in thousandths: the covered leases are charged at least this share of their
    /// predicted cores. Default 750; kept within 500 to 1000.
    pub cpu_floor_permille: u32,
    /// `s_cpu`, in thousandths: the CPU the predicate lets a worker be charged above its
    /// capacity. Default 100.
    pub s_cpu_permille: u32,
    /// `s_mem`, in thousandths. Default 0.
    pub s_mem_permille: u32,
    /// The most cold leases admitted per worker per window; `None` is one per 32 cores
    /// of the worker, rounded up. At least 1.
    pub cold_per_window: Option<u32>,
    /// How much of a worker's memory, in thousandths, its active file pages may count for
    /// in the working set (see `WorkerLoad::working_set_counted`). Default 250; at most
    /// 1000.
    pub file_share_permille: u32,
}

impl Default for LoadConfig {
    fn default() -> Self {
        Self {
            mode: PlacementLoad::On,
            window: Duration::from_secs(5),
            cpu_floor_permille: 750,
            s_cpu_permille: 100,
            s_mem_permille: 0,
            cold_per_window: None,
            file_share_permille: 250,
        }
    }
}

impl LoadConfig {
    /// These settings with each value brought into its range.
    #[must_use]
    pub fn normalised(self) -> Self {
        Self {
            window: self.window.max(Duration::from_millis(1)),
            cpu_floor_permille: self.cpu_floor_permille.clamp(500, 1000),
            cold_per_window: self.cold_per_window.map(|n| n.max(1)),
            file_share_permille: self.file_share_permille.min(1000),
            ..self
        }
    }

    fn window_millis(&self) -> u64 {
        u64::try_from(self.window.as_millis())
            .unwrap_or(u64::MAX)
            .max(1)
    }

    pub(crate) fn window_usec(&self) -> u64 {
        self.window_millis().saturating_mul(1000)
    }

    /// The number of the window `now` falls in.
    pub(crate) fn window_index(&self, now: FarmTime) -> u64 {
        now.as_millis() / self.window_millis()
    }

    /// `f * cores`, rounded up.
    pub(crate) fn floor_of(&self, cores: u64) -> u64 {
        scale_up(cores, u64::from(self.cpu_floor_permille), 1000)
    }

    /// `C_cpu * (1 + s_cpu)`.
    pub(crate) fn cpu_limit(&self, capacity: &Resources) -> u64 {
        scale(
            capacity.cpu_millis,
            1000 + u64::from(self.s_cpu_permille),
            1000,
        )
    }

    /// `C_mem * (1 + s_mem)`.
    pub(crate) fn mem_limit(&self, capacity: &Resources) -> u64 {
        scale(
            capacity.memory_bytes,
            1000 + u64::from(self.s_mem_permille),
            1000,
        )
    }

    /// The cold leases a worker of `capacity` admits per window.
    pub(crate) fn cold_cap(&self, capacity: &Resources) -> u32 {
        self.cold_per_window.map_or_else(
            || {
                u32::try_from(capacity.cpu_millis.div_ceil(32_000))
                    .unwrap_or(u32::MAX)
                    .max(1)
            },
            |n| n.max(1),
        )
    }
}

fn scale(value: u64, num: u64, den: u64) -> u64 {
    u64::try_from(u128::from(value) * u128::from(num) / u128::from(den)).unwrap_or(u64::MAX)
}

fn scale_up(value: u64, num: u64, den: u64) -> u64 {
    u64::try_from((u128::from(value) * u128::from(num)).div_ceil(u128::from(den)))
        .unwrap_or(u64::MAX)
}

/// What admission has done since the scheduler was built. Plain counts for the caller to
/// export; a count is of a decision, not of a lease, where it says so.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LoadCounters {
    /// Leases the predicate admitted (mode `On`).
    pub admitted: u64,
    /// Of those, on a worker charged by prediction alone.
    pub admitted_fallback: u64,
    /// Of those, cold ones.
    pub admitted_cold: u64,
    /// Of those, whose key has a curve that the flat rule did not use.
    pub curve_flat: u64,
    /// Queued requests, one count per request per round, that had a worker with room
    /// apart from the predicate and found none it admitted.
    pub blocked: u64,
    /// Of those, requests whose only obstacle was the cold cap.
    pub cold_deferred: u64,
    /// Head reservations taken or moved.
    pub reservations: u64,
    /// Shadow: placements the predicate would have admitted on the worker chosen.
    pub would_admit: u64,
    /// Shadow: placements the predicate would have refused on the worker chosen.
    pub would_block: u64,
    /// Readings accepted.
    pub readings_accepted: u64,
    /// Readings dropped: out of order, a counter or the clock going back, or implausible.
    pub readings_dropped: u64,
    /// Accepted readings that gave a window: the denominator of the share of windows
    /// in which the floor binds.
    pub floor_windows: u64,
    /// Of those, windows in which `f * sum p(covered)` was above the measurement.
    pub floor_binds: u64,
    /// Not a running total: the live workers, as of the call that returned this, that
    /// are charged by prediction because their newest reading says the actions cgroup
    /// has a CPU limit, while the load rule is on. Part of
    /// [`crate::Scheduler::fallback_workers`]; a count above zero is a node whose cgroup
    /// limit and pool do not match.
    pub fallback_cpu_limited: u64,
}

/// A prediction of what an action costs, from the estimator, fed for an operation with
/// [`crate::Scheduler::set_cost`]. An operation with none is charged its request's cores
/// and no memory: the booked rule's figures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Prediction {
    /// Predicted cores, in millicores; sub-core allowed.
    pub cpu_millis: u64,
    /// Predicted memory peak, in bytes.
    pub memory_bytes: u64,
    /// The key had no usable estimate and these are the cold costs.
    pub cold: bool,
}

impl Prediction {
    /// The prediction of a learned estimate.
    #[must_use]
    pub fn from_estimate(estimate: &Estimate) -> Self {
        Self {
            cpu_millis: u64::from(estimate.cpu_millis),
            memory_bytes: estimate.memory_bytes,
            cold: false,
        }
    }

    /// The cost of a key with no estimate: [`COLD_CPU_MILLIS`] cores and `default_memory`
    /// bytes (the first rung of the ladder), counted against the cold cap.
    #[must_use]
    pub fn cold(default_memory: u64) -> Self {
        Self {
            cpu_millis: u64::from(COLD_CPU_MILLIS),
            memory_bytes: default_memory,
            cold: true,
        }
    }
}

/// What a lease is charged in the sums, by one lease.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Charge {
    pub(crate) cpu_millis: u64,
    pub(crate) memory_bytes: u64,
}

/// The charge of a lease: its predicted cores and its predicted memory peak, whole for
/// its whole life. The one place a lease's charge is computed; a rule that charges by a
/// curve over the lease's minutes replaces this function.
pub(crate) const fn charge(cost: &LeaseCost) -> Charge {
    Charge {
        cpu_millis: cost.cpu_millis,
        memory_bytes: cost.memory_bytes,
    }
}

/// The curves the scheduler was given; the flat rule does not read them.
#[derive(Clone)]
struct Curves(Arc<dyn CurveSource + Send + Sync>);

impl fmt::Debug for Curves {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Curves")
    }
}

/// How a request that was not placed was held back by the load.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Blocked {
    /// Some worker had room apart from the predicate, and the predicate refused each on
    /// CPU or memory.
    Load,
    /// As above, and only the cold cap stood in the way somewhere and nowhere else.
    ColdCap,
}

/// The scheduler's admission state: settings, counters, the cost predictions fed for
/// queued operations, and the curve source.
#[derive(Clone, Debug)]
pub(crate) struct Admission {
    pub(crate) cfg: LoadConfig,
    pub(crate) counters: LoadCounters,
    hints: BTreeMap<OperationId, Prediction>,
    curves: Curves,
}

impl Default for Admission {
    /// A scheduler is built admitting by the booked rule, whatever [`LoadConfig::default`]
    /// says: the caller turns the load predicate on with
    /// [`crate::Scheduler::with_load_config`], so that a scheduler built as before places
    /// as before.
    fn default() -> Self {
        Self {
            cfg: LoadConfig {
                mode: PlacementLoad::Off,
                ..LoadConfig::default()
            },
            counters: LoadCounters::default(),
            hints: BTreeMap::new(),
            curves: Curves(Arc::new(NoCurves)),
        }
    }
}

impl Admission {
    pub(crate) fn configure(&mut self, cfg: LoadConfig) {
        self.cfg = cfg.normalised();
    }

    pub(crate) fn use_curves(&mut self, curves: Arc<dyn CurveSource + Send + Sync>) {
        self.curves = Curves(curves);
    }

    /// Whether placement is decided by the predicate.
    pub(crate) fn by_load(&self) -> bool {
        self.cfg.mode == PlacementLoad::On
    }

    pub(crate) fn set_prediction(&mut self, operation: OperationId, prediction: Prediction) {
        self.hints.insert(operation, prediction);
    }

    pub(crate) fn forget(&mut self, operation: OperationId) {
        self.hints.remove(&operation);
    }

    /// The cost `request`, operation `operation`, is charged if placed now: its
    /// prediction (else its request's cores and no memory), the limit it runs under, and
    /// for a retry up the memory ladder (`retried`) at least that limit as memory: a
    /// spiky key killed at its learned size is not counted at it again.
    pub(crate) fn cost_of(
        &self,
        operation: OperationId,
        request: &Request,
        retried: bool,
    ) -> LeaseCost {
        if request.kind == LeaseKind::Vm {
            return Self::booked_cost(request);
        }
        let limit = request.resources.memory_bytes;
        let (cpu, memory, cold) = match self.hints.get(&operation) {
            Some(p) => (p.cpu_millis, p.memory_bytes, p.cold),
            None => (request.resources.cpu_millis, 0, false),
        };
        LeaseCost {
            cpu_millis: cpu,
            memory_bytes: if retried { memory.max(limit) } else { memory },
            limit_bytes: limit,
            cold,
            retry: retried,
            key_hash: key_hash(&request.key),
        }
    }

    /// The cost a lease of `request` is charged when no prediction is known: a lease taken
    /// over from a store written before costs were kept, or a VM, which is charged what
    /// it books.
    pub(crate) fn booked_cost(request: &Request) -> LeaseCost {
        let r = &request.resources;
        match request.kind {
            LeaseKind::Vm => LeaseCost {
                cpu_millis: r.cpu_millis,
                memory_bytes: r.memory_bytes,
                limit_bytes: r.memory_bytes,
                cold: false,
                retry: false,
                key_hash: key_hash(&request.key),
            },
            _ => LeaseCost {
                cpu_millis: r.cpu_millis,
                memory_bytes: 0,
                limit_bytes: r.memory_bytes,
                cold: false,
                retry: false,
                key_hash: key_hash(&request.key),
            },
        }
    }

    /// Counts a lease the predicate admitted.
    pub(crate) fn count_admitted(&mut self, key: &WorkKey, cost: &LeaseCost, fallback: bool) {
        let c = &mut self.counters;
        c.admitted += 1;
        c.admitted_fallback += u64::from(fallback);
        c.admitted_cold += u64::from(cost.cold);
        // The flat rule charges a key that has a curve flat: say how often.
        c.curve_flat += u64::from(self.curves.0.curve(key).is_some());
    }

    /// Counts what the predicate said about the worker `Shadow` placed on.
    pub(crate) fn count_shadow(&mut self, admitted: bool) {
        if admitted {
            self.counters.would_admit += 1;
        } else {
            self.counters.would_block += 1;
        }
    }

    pub(crate) fn count_blocked(&mut self, why: Blocked) {
        self.counters.blocked += 1;
        self.counters.cold_deferred += u64::from(why == Blocked::ColdCap);
    }
}

/// How `worker` is charged right now, for the operator and for tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoadView {
    /// No usable measurement: every lease is charged its full predicted cost.
    pub fallback: bool,
    /// Cores busy, in millicores, over the last two windows (the larger); `None` before
    /// the first window.
    pub measured_cpu_millis: Option<u32>,
    /// The newest reading's working set, as reported.
    pub working_set_bytes: Option<u64>,
    /// The CPU the predicate charges the worker before a new lease, in millicores.
    pub left_cpu_millis: u64,
    /// The memory it charges, in bytes.
    pub left_memory_bytes: u64,
    /// Predicted cores of every action lease, in millicores.
    pub predicted_cpu_millis: u64,
    /// Of those, the leases no reading has covered yet.
    pub uncovered_cpu_millis: u64,
    /// Predicted memory of every action lease, in bytes.
    pub predicted_memory_bytes: u64,
    /// Of those, the leases no reading has covered yet.
    pub uncovered_memory_bytes: u64,
    /// Booked CPU and memory of VM leases, added to every sum.
    pub vm_cpu_millis: u64,
    /// Booked memory of VM leases.
    pub vm_memory_bytes: u64,
    /// Cold leases admitted in the current window.
    pub cold_this_window: u32,
}

impl LoadView {
    pub(crate) fn of(
        load: &WorkerLoad,
        capacity: &Resources,
        now: FarmTime,
        cfg: &LoadConfig,
    ) -> Self {
        let left = load.left(now, capacity, cfg);
        let t = load.totals();
        Self {
            fallback: left.fallback,
            measured_cpu_millis: load.measured(),
            working_set_bytes: load.working_set(),
            left_cpu_millis: left.cpu,
            left_memory_bytes: left.mem,
            predicted_cpu_millis: t.cpu_all(),
            uncovered_cpu_millis: t.cpu_uncovered,
            predicted_memory_bytes: t.mem_all(),
            uncovered_memory_bytes: t.mem_uncovered,
            vm_cpu_millis: t.vm_cpu,
            vm_memory_bytes: t.vm_mem,
            cold_this_window: load.cold_admitted(now, cfg),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: settings out of range reaching the predicate: a floor below a half or above
    /// one, a zero window (every window the same index, a zero staleness limit), a
    /// cold cap of 0 (no cold key ever runs) and a file share above the whole node.
    #[test]
    fn settings_are_brought_into_range() {
        let wild = LoadConfig {
            window: Duration::ZERO,
            cpu_floor_permille: 10,
            cold_per_window: Some(0),
            file_share_permille: 9_000,
            ..LoadConfig::default()
        }
        .normalised();
        assert_eq!(wild.window, Duration::from_millis(1));
        assert_eq!(wild.cpu_floor_permille, 500);
        assert_eq!(wild.cold_per_window, Some(1));
        assert_eq!(wild.file_share_permille, 1000);
        let high = LoadConfig {
            cpu_floor_permille: 5_000,
            ..LoadConfig::default()
        }
        .normalised();
        assert_eq!(high.cpu_floor_permille, 1000);
        assert_eq!(LoadConfig::default().normalised(), LoadConfig::default());
    }

    /// Catches: the default cold cap not being one per 32 cores rounded up (3 on 88
    /// threads), a worker of under 32 cores getting none, and an explicit setting being
    /// ignored.
    #[test]
    fn the_cold_cap_is_one_per_32_cores_rounded_up() {
        let cfg = LoadConfig::default();
        let cap = |cores: u64| cfg.cold_cap(&Resources::new(cores * 1000, 0));
        assert_eq!(cap(88), 3);
        assert_eq!(cap(64), 2);
        assert_eq!(cap(65), 3);
        assert_eq!(cap(16), 1);
        assert_eq!(cap(0), 1);
        let set = LoadConfig {
            cold_per_window: Some(12),
            ..cfg
        };
        assert_eq!(set.cold_cap(&Resources::new(88_000, 0)), 12);
    }

    /// Catches: limits computed with the slack on the wrong resource, overflowing, or
    /// the floor rounded down (an under-charge).
    #[test]
    fn limits_floor_and_window_arithmetic() {
        let cfg = LoadConfig::default();
        let cap = Resources::new(88_000, 1000);
        assert_eq!(cfg.cpu_limit(&cap), 96_800);
        assert_eq!(cfg.mem_limit(&cap), 1000);
        let loose = LoadConfig {
            s_mem_permille: 500,
            ..cfg
        };
        assert_eq!(loose.mem_limit(&cap), 1500);
        assert_eq!(cfg.cpu_limit(&Resources::new(u64::MAX, 0)), u64::MAX);
        assert_eq!(cfg.floor_of(1000), 750);
        assert_eq!(cfg.floor_of(1), 1, "rounded up");
        assert_eq!(cfg.floor_of(u64::MAX), 13_835_058_055_282_163_712);
        assert_eq!(cfg.window_usec(), 5_000_000);
        assert_eq!(cfg.window_index(FarmTime::from_millis(4_999)), 0);
        assert_eq!(cfg.window_index(FarmTime::from_millis(5_000)), 1);
    }

    /// Catches: a prediction built from the wrong estimator field, or a cold prediction
    /// that is not the cold cores and the first rung.
    #[test]
    fn predictions_come_from_the_estimator() {
        let estimate = Estimate {
            memory_bytes: 3 << 30,
            cpu_millis: 750,
            level: js_sizing::Level::L1,
        };
        assert_eq!(
            Prediction::from_estimate(&estimate),
            Prediction {
                cpu_millis: 750,
                memory_bytes: 3 << 30,
                cold: false,
            }
        );
        assert_eq!(
            Prediction::cold(4 << 30),
            Prediction {
                cpu_millis: 2000,
                memory_bytes: 4 << 30,
                cold: true,
            }
        );
    }
}
