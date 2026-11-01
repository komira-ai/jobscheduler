//! Admission by measured and predicted load: the flat rule of the placement-by-load
//! design.
//!
//! An action is admitted to a worker when the worker's load, plus the predicted cost of
//! what the measurement does not yet cover, plus the predicted cost of the new lease,
//! stays within the worker's capacity, for CPU and for memory separately. With `C` the
//! worker's capacity (its reported memory cut to `floor(memory * percent / 100)` by
//! [`Scheduler::with_memory_percent`](crate::Scheduler::with_memory_percent), 100 by
//! default), `p` the cores and `m` the memory peak a lease was charged:
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
//! - Time profiles (`profile.rs`, `horizon.rs`): a key with a learned curve (3 runs or
//!   more, not demoted) is charged per minute of the lease's life instead of one number, and
//!   the predicate is checked at every minute of the next 30 and once for what each lease
//!   asks for from then on. The measurement enters at the first minute only. Cold keys,
//!   demoted keys, retries and workers charged by prediction alone are charged flat, as
//!   before. The switches are independent ([`LoadConfig::profile`] for CPU,
//!   [`LoadConfig::profile_memory`] for memory): a resource whose switch is off is charged
//!   flat, and both off give the flat rule exactly.
//! - The shape guard (`guard.rs`) charges a resource of a key flat for 8 runs when its
//!   curve misfit in 2 of the last 3 runs or 3 of the last 10.
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
//! Where a lease's flat charge is computed there is one function, [`charge`]; the charge by
//! a curve over the lease's minutes is [`profile::Profile`], and the two are summed in the
//! ledger by one rule ([`horizon`]). The curve of a lease is not part of its cost record:
//! a lease recovered from the store is charged flat until it ends.

mod config;
mod guard;
mod head;
mod horizon;
mod ledger;
mod profile;

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use js_sizing::{COLD_CPU_MILLIS, Estimate};
use js_types::sched::usage::{CurveSource, LeaseCost, NoCurves, RunEnd, key_hash};
use js_types::sched::{FarmTime, LeaseKind, OperationId, Resources, WorkKey};

pub use config::{FlagError, LoadConfig, MAX_CPU_SLACK_PERMILLE, PlacementLoad};
pub(crate) use config::{scale, scale_up};
pub(crate) use head::{Ratio, binding, pick, start_time};
pub(crate) use horizon::new_charge;
pub(crate) use ledger::{Ended, Left, Refusal, WorkerLoad, clamped};
pub use profile::HORIZON_MINUTES;
pub(crate) use profile::{H_PLUS as H_PLUS_SLOT, Profile};

use crate::input::Request;
use crate::load::{Charge, ScoreConfig, Scorer};

use guard::{Resource, ShapeGuard};
use profile::{Choice, MIN_RUNS};

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
    /// Of those, whose key has a curve that was not used: charged flat in both CPU and
    /// memory (the switches are off, the key is demoted, or it is a retry).
    pub curve_flat: u64,
    /// Of those, charged by curve in CPU.
    pub profile_cpu: u64,
    /// Of those, charged by curve in memory.
    pub profile_memory: u64,
    /// Lease-minutes whose mean cores were over twice the charge (and 250 m over it).
    pub misfit_cpu_minutes: u64,
    /// Lease-minutes whose working set was above the curve's remaining maximum.
    pub misfit_memory_minutes: u64,
    /// Times the shape guard charged a key's CPU flat for the next runs.
    pub demotions_cpu: u64,
    /// Times the shape guard charged a key's memory flat for the next runs.
    pub demotions_memory: u64,
    /// Queued requests, one count per request per round, that had a worker with room
    /// apart from the predicate and found none it admitted.
    pub blocked: u64,
    /// Of those, requests whose only obstacle was the cold cap.
    pub cold_deferred: u64,
    /// Head reservations taken or moved.
    pub reservations: u64,
    /// Readings accepted.
    pub readings_accepted: u64,
    /// Readings dropped: out of order, a counter or the clock going back, or implausible.
    pub readings_dropped: u64,
    /// Accepted readings that gave a window: the denominator of the share of windows
    /// in which the floor binds.
    pub floor_windows: u64,
    /// Of those, windows in which `f * sum p(covered)` was above the measurement.
    pub floor_binds: u64,
    /// Slot-resource evaluations of the score: 64 per scored worker per request. Zero for a
    /// worker the predicate refused or that was excluded before it, and for a request with
    /// one worker left.
    pub score_evaluations: u64,
    /// Placements chosen by the score among two or more workers.
    pub scored: u64,
    /// Of those, placements on a worker predicted above its capacity (plus the tolerance):
    /// key 1, the slack zone was used.
    pub chose_slack: u64,
    /// Of those, placements on a distrusted worker (key 3).
    pub chose_distrusted: u64,
    /// Of those, placements on a worker concentrated on the lease's key (key 4).
    pub chose_concentrated: u64,
    /// Of those, placements where the best two were equal in keys 1 to 5 and a tie-break
    /// decided.
    pub chose_by_tie_break: u64,
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
    /// `L`, the key's life in minutes (`js_sizing::Summary::life_minutes`), 1 for a cold
    /// key. The score's fit reads it only for a lease charged by a curve.
    pub life_minutes: u8,
}

impl Prediction {
    /// The prediction of a learned estimate.
    #[must_use]
    pub fn from_estimate(estimate: &Estimate) -> Self {
        Self {
            cpu_millis: u64::from(estimate.cpu_millis),
            memory_bytes: estimate.memory_bytes,
            cold: false,
            life_minutes: estimate.life_minutes,
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
            life_minutes: 1,
        }
    }
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

/// The minutes of the lease's own life the score's fit looks at (`L`): the key's
/// `life_minutes`, clamped to `1..=HORIZON`, where the lease is charged by a curve; 1 where
/// it is charged flat.
pub(crate) fn fit_minutes_of(life_minutes: u8, charged_by_curve: bool) -> u8 {
    if charged_by_curve {
        life_minutes.clamp(1, u8::try_from(crate::load::HORIZON).unwrap_or(u8::MAX))
    } else {
        1
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
    guard: ShapeGuard,
}

impl Default for Admission {
    /// A scheduler is built admitting by load, as [`LoadConfig::default`] says.
    fn default() -> Self {
        Self {
            cfg: LoadConfig::default(),
            counters: LoadCounters::default(),
            hints: BTreeMap::new(),
            curves: Curves(Arc::new(NoCurves)),
            guard: ShapeGuard::default(),
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

    /// The P90 of the wall minutes the runs of `key` lasted, when the curve source knows
    /// it. Read whatever the load settings are: the drain cost uses it for the tail.
    pub(crate) fn wall_minutes_p90(&self, key: &WorkKey) -> Option<u32> {
        self.curves.0.wall_minutes_p90(key)
    }

    /// Whether placement is decided by the predicate.
    pub(crate) fn by_load(&self) -> bool {
        self.cfg.mode == PlacementLoad::On
    }

    pub(crate) fn set_prediction(&mut self, operation: OperationId, prediction: Prediction) {
        self.hints.insert(operation, prediction);
    }

    /// `L` for the fit of `operation`'s lease: the key's life where the lease is charged by
    /// a curve (in CPU or in memory), 1 where it is charged flat.
    pub(crate) fn fit_minutes(&self, operation: OperationId, profile: Option<&Profile>) -> u8 {
        let life = self.hints.get(&operation).map_or(1, |p| p.life_minutes);
        fit_minutes_of(life, profile.is_some_and(Profile::by_curve))
    }

    pub(crate) fn forget(&mut self, operation: OperationId) {
        self.hints.remove(&operation);
    }

    /// Whether a prediction is held for `operation`.
    pub(crate) fn hinted(&self, operation: OperationId) -> bool {
        self.hints.contains_key(&operation)
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

    /// How the lease of `operation`, `request`, charged `cost` if placed now, is charged
    /// over time: by its key's curve (in CPU, in memory, or both), up to an expected life,
    /// or flat, which is `None`.
    ///
    /// A key is charged by curve in CPU when [`LoadConfig::profile`] is on and in memory
    /// when [`LoadConfig::profile_memory`] is on, independently: a switch that is off
    /// charges that resource flat. Either way the key has a prediction that is not
    /// cold, its curve was fitted from [`MIN_RUNS`] runs or more, the lease is not a
    /// retry up the memory ladder (charged flat at its rung), and the shape guard has not
    /// demoted the resource.
    pub(crate) fn profile_of(
        &self,
        operation: OperationId,
        request: &Request,
        cost: &LeaseCost,
        guard_key: u64,
    ) -> Option<Arc<Profile>> {
        // A scheduler that places by the booked rule reads no curve.
        if request.kind != LeaseKind::Action || cost.retry || self.cfg.mode == PlacementLoad::Off {
            return None;
        }
        let cfg = &self.cfg;
        self.hints.get(&operation).filter(|p| !p.cold)?;
        // CPU and memory each follow their own switch: a curve is read when either is on.
        let curve = (cfg.profile || cfg.profile_memory)
            .then(|| self.curves.0.curve(&request.key))
            .flatten()
            .filter(|c| c.runs() >= MIN_RUNS && !c.points().is_empty());
        let life = cfg.expected_life_delta_minutes.and_then(|delta| {
            self.curves
                .0
                .wall_minutes_p90(&request.key)
                .map(|minutes| minutes.saturating_add(delta))
        });
        if curve.is_none() && life.is_none() {
            return None;
        }
        let (cpu_left, mem_left) = self.guard.runs_left(guard_key);
        let charged = curve.is_some();
        let choice = Choice {
            curve,
            cpu: cfg.profile && cpu_left == 0,
            mem: cfg.profile_memory && mem_left == 0,
            demoted_cpu: charged && cfg.profile && cpu_left > 0,
            demoted_mem: charged && cfg.profile_memory && mem_left > 0,
            life,
            floor_permille: cfg.profile_floor_permille,
            guard_key,
        };
        Some(Arc::new(Profile::new(choice, cost)))
    }

    /// The runs left that the shape guard charges `guard_key`'s CPU and memory flat.
    pub(crate) fn shape_runs_left(&self, guard_key: u64) -> (u32, u32) {
        self.guard.runs_left(guard_key)
    }

    /// A lease taken off its worker's ledger ended as `end`: its run is judged by the shape
    /// guard if it was charged by curve (a run that did not run to its end, or was killed
    /// by the node, says nothing about the curve), or counted off the flat runs of a
    /// demoted key.
    pub(crate) fn run_ended(&mut self, ended: &Ended, end: RunEnd) {
        let Some(p) = ended.profile.as_deref() else {
            return;
        };
        let key = p.guard_key;
        let ran = matches!(end, RunEnd::Clean | RunEnd::KilledAtLimit);
        if p.demoted_cpu {
            if ran {
                self.guard.flat_run(key, Resource::Cpu);
            }
        } else if p.cpu_by_curve()
            && end == RunEnd::Clean
            && self.guard.record(key, Resource::Cpu, ended.cpu_misfit)
        {
            self.counters.demotions_cpu += 1;
        }
        if p.demoted_mem {
            if ran {
                self.guard.flat_run(key, Resource::Memory);
            }
        } else if p.mem_by_curve() {
            let misfit = match end {
                RunEnd::Clean => Some(ended.mem_misfit),
                // A kill at the lease's own limit is a misfit by itself.
                RunEnd::KilledAtLimit => Some(true),
                _ => None,
            };
            if let Some(misfit) = misfit
                && self.guard.record(key, Resource::Memory, misfit)
            {
                self.counters.demotions_memory += 1;
            }
        }
    }

    /// `cost` with the key hash of `request`'s key if it has none and the request is an
    /// action: a cost record written before the key hash was kept is rebuilt with it.
    pub(crate) fn with_key_hash(cost: LeaseCost, request: &Request) -> LeaseCost {
        if cost.key_hash == 0 && request.kind == LeaseKind::Action {
            LeaseCost {
                key_hash: key_hash(&request.key),
                ..cost
            }
        } else {
            cost
        }
    }

    /// Counts a lease the predicate admitted.
    pub(crate) fn count_admitted(
        &mut self,
        key: &WorkKey,
        cost: &LeaseCost,
        fallback: bool,
        profile: Option<&Profile>,
    ) {
        let c = &mut self.counters;
        c.admitted += 1;
        c.admitted_fallback += u64::from(fallback);
        c.admitted_cold += u64::from(cost.cold);
        // On a worker charged by prediction alone, a curve is not used either.
        let by_curve = profile.filter(|_| !fallback);
        let cpu = by_curve.is_some_and(Profile::cpu_by_curve);
        let memory = by_curve.is_some_and(Profile::mem_by_curve);
        c.profile_cpu += u64::from(cpu);
        c.profile_memory += u64::from(memory);
        // A key with a curve that was charged flat: say how often.
        c.curve_flat += u64::from(!cpu && !memory && self.curves.0.curve(key).is_some());
    }

    /// The scorer for the current settings.
    pub(crate) fn scorer(&self) -> Scorer {
        Scorer::new(ScoreConfig::of(&self.cfg))
    }

    /// Counts a placement the score made: the evaluations it spent and how it decided.
    pub(crate) fn count_scored(&mut self, evaluations: u64, choice: &crate::load::Choice<'_>) {
        let c = &mut self.counters;
        c.score_evaluations += evaluations;
        if choice.scored {
            c.scored += 1;
            c.chose_slack += u64::from(choice.slack_used);
            c.chose_distrusted += u64::from(choice.distrusted);
            c.chose_concentrated += u64::from(choice.concentrated);
            c.chose_by_tie_break += u64::from(choice.tie_break);
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
    use js_types::sched::WorkerId;

    use super::*;

    /// Catches: a key's life read by the fit of a lease charged flat (a flat lease looks at
    /// its first minute only), and a curve-charged life outside `1..=HORIZON`.
    #[test]
    fn the_fit_reads_the_life_only_where_a_curve_charges() {
        assert_eq!(fit_minutes_of(9, false), 1);
        assert_eq!(fit_minutes_of(9, true), 9);
        assert_eq!(fit_minutes_of(0, true), 1);
        assert_eq!(fit_minutes_of(200, true), 30);
        assert_eq!(usize::from(js_sizing::LIFE_HORIZON), crate::load::HORIZON);
    }

    /// Catches: the score's fit reading the key's life for a lease charged flat (a life
    /// in minutes it is not charged over), or reading one minute for a lease charged by a
    /// curve in CPU alone, in memory alone, or in both.
    #[test]
    fn the_fit_looks_at_the_keys_life_only_for_a_lease_charged_by_a_curve() {
        use js_types::sched::usage::{Curve, MinutePoint};
        let mut adm = Admission::default();
        let op = OperationId(1);
        adm.set_prediction(
            op,
            Prediction {
                cpu_millis: 1_000,
                memory_bytes: 0,
                cold: false,
                life_minutes: 9,
            },
        );
        let profile = |by_curve: bool, cpu: bool, mem: bool, life: Option<u32>| {
            Profile::new(
                profile::Choice {
                    curve: by_curve
                        .then(|| Curve::new(vec![MinutePoint::default()], 5).expect("one point")),
                    cpu,
                    mem,
                    demoted_cpu: false,
                    demoted_mem: false,
                    life,
                    floor_permille: 750,
                    guard_key: 0,
                },
                &LeaseCost::default(),
            )
        };
        let fit = |p: Option<&Profile>| adm.fit_minutes(op, p);
        assert_eq!(fit(Some(&profile(true, true, true, None))), 9);
        assert_eq!(fit(Some(&profile(true, true, false, None))), 9, "CPU alone");
        assert_eq!(
            fit(Some(&profile(true, false, true, None))),
            9,
            "memory alone"
        );
        assert_eq!(fit(Some(&profile(true, false, false, None))), 1, "demoted");
        assert_eq!(
            fit(Some(&profile(false, true, true, Some(3)))),
            1,
            "a life only"
        );
        assert_eq!(fit(None), 1);
        assert_eq!(adm.fit_minutes(OperationId(2), None), 1, "no prediction");
    }

    /// Catches: a prediction built from the wrong estimator field, or a cold prediction
    /// that is not the cold cores and the first rung.
    #[test]
    fn predictions_come_from_the_estimator() {
        let estimate = Estimate {
            memory_bytes: 3 << 30,
            cpu_millis: 750,
            level: js_sizing::Level::L1,
            life_minutes: 4,
        };
        assert_eq!(
            Prediction::from_estimate(&estimate),
            Prediction {
                cpu_millis: 750,
                memory_bytes: 3 << 30,
                cold: false,
                life_minutes: 4,
            }
        );
        assert_eq!(
            Prediction::cold(4 << 30),
            Prediction {
                cpu_millis: 2000,
                memory_bytes: 4 << 30,
                cold: true,
                life_minutes: 1,
            }
        );
    }

    fn request(kind: LeaseKind) -> Request {
        Request {
            key: WorkKey::new("load", "k"),
            qos: js_types::sched::Qos::Ci,
            kind,
            resources: Resources::new(1_000, 1 << 30),
            needs: js_caps::Request::default(),
            hermetic: true,
            do_not_cache: false,
            whole_worker: false,
        }
    }

    /// Catches: a recovered action whose cost record has no key hash left at 0 (every
    /// such lease would share one key's 25% share), and a hash written over a cost that
    /// has one, or onto a lease that is not an action.
    #[test]
    fn a_cost_with_no_key_hash_takes_the_requests() {
        let action = request(LeaseKind::Action);
        let cost = Admission::booked_cost(&action);
        let unhashed = LeaseCost {
            key_hash: 0,
            ..cost
        };
        assert_eq!(Admission::with_key_hash(unhashed, &action), cost);
        let other = LeaseCost {
            key_hash: 7,
            ..cost
        };
        assert_eq!(Admission::with_key_hash(other, &action), other);
        let whole = request(LeaseKind::WholeMachine);
        assert_eq!(Admission::with_key_hash(unhashed, &whole), unhashed);
    }

    /// Catches: a choice that was not scored (the only candidate) counted as scored or
    /// by its keys, and the evaluations of a scored choice not added.
    #[test]
    fn only_a_scored_choice_counts_how_it_was_decided() {
        let worker = WorkerId::new("a");
        let choice = crate::load::Choice {
            worker: &worker,
            scored: false,
            slack_used: true,
            distrusted: true,
            concentrated: true,
            tie_break: true,
        };
        let mut adm = Admission::default();
        adm.count_scored(0, &choice);
        assert_eq!(adm.counters, LoadCounters::default());
        adm.count_scored(
            128,
            &crate::load::Choice {
                scored: true,
                ..choice
            },
        );
        let c = &adm.counters;
        assert_eq!(
            (
                c.score_evaluations,
                c.scored,
                c.chose_slack,
                c.chose_distrusted,
                c.chose_concentrated,
                c.chose_by_tie_break
            ),
            (128, 1, 1, 1, 1, 1)
        );
    }

    /// Catches: the fit shape a config is built with not being the one it reports.
    #[test]
    fn a_config_reports_its_fit_shape() {
        use crate::load::FitShape;
        assert_eq!(ScoreConfig::default().fit(), FitShape::Squared);
        for fit in [FitShape::Squared, FitShape::Linear, FitShape::Flat] {
            assert_eq!(ScoreConfig::default().with_fit(fit).fit(), fit);
        }
    }
}
