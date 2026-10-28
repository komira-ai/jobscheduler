//! The adapter from the replay's model to the scorer's seam, and ranker 5 on top of it.
//!
//! The scorer reads costs and load through [`PredictedLoad`]. The replay holds, per host,
//! the figures the admission stand-in already keeps: the sum of the resident beliefs at each
//! later slot, and at slot 0 the pieces of the measured rule. This module turns those into
//! the four answers of the seam and makes no decision of its own.
//!
//! * `load(w, 0)` is the predicate's left side at slot 0, measurement included:
//!   `max(measured, f * covered) + uncovered` for CPU and
//!   `max(all, working_set + uncovered)` for memory. [`slot0_left`] is that formula, the one
//!   place it is written, and it tells whether the measurement term was the larger.
//! * `load(w, slot)` for the later slots is the host's vector at that slot.
//! * `charge(w, slot)` is the lease's belief charge at that slot, the same on every host.
//! * `host(w)` is the scalars: capacities, `L` of the lease, the key figures that key 4
//!   reads, the number of uncovered leases, and whether the measurement is above the
//!   prediction. A replay host always has a measurement, so `fallback` is false.
//!
//! [`ScoreRanker`] is ranker 5: [`Scorer::choose`] over the candidates the predicate passed,
//! plus the counters the report and the evaluation identity read. It imports nothing from
//! the other engine modules; the engine builds a [`ScoreModel`] from its own state.

use std::collections::BTreeMap;

use js_sched::{Charge, HostFacts, LeaseFacts, PredictedLoad, SLOTS, ScoreConfig, Scorer};
use js_types::sched::{LeaseId, WorkerId};

/// The term of every minted lease id. The tie-break hashes `term-seq` text, so this is part
/// of the pinned order: changing it changes which host an equal score goes to.
pub const LEASE_TERM: u64 = 1;

/// `f` of the shipped predicate, in thousandths: covered leases are charged at least this
/// share of their predicted cores.
pub const FLOOR_PERMILLE: u64 = 750;

/// The scorer's lease id for the replay's `lease_ix`: a fixed term and the index as the
/// sequence. The same index gives the same id in every run and in every ranker.
#[must_use]
pub const fn lease_id(lease_ix: u64) -> LeaseId {
    LeaseId {
        term: LEASE_TERM,
        seq: lease_ix,
    }
}

/// What the lease being placed is believed to cost. The belief is a function of the fixture
/// and the seed, so it is the same on every host and for every ranker.
#[derive(Clone, Copy, Debug)]
pub struct LeaseBelief {
    /// The replay's index of the lease; minted into its [`LeaseId`].
    pub lease_ix: u64,
    /// The key has fewer than three usable runs: charged flat, memory held to the larger
    /// miss factor.
    pub cold: bool,
    /// `L`: the leading minutes the fit looks at. 1 for a cold lease.
    pub fit_minutes: u8,
    /// The lease's flat CPU cost `p_bar`, in millicores; what key 4 adds for the lease.
    pub p_bar_cpu_millis: u64,
    /// The charge at each slot: minutes `0..=30`, then the `H+` slot.
    pub charge: [Charge; SLOTS],
}

/// The pieces of the predicate's left side at slot 0, for one host.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Slot0 {
    /// The larger of the last two windows of measured CPU, in millicores.
    pub measured_cpu_millis: u64,
    /// The predicted CPU of the leases a full window has covered.
    pub covered_cpu_millis: u64,
    /// The predicted CPU of the leases no full window has covered.
    pub uncovered_cpu_millis: u64,
    /// The predicted memory of every resident, in bytes.
    pub memory_all_bytes: u64,
    /// The newest reading's working set, in bytes.
    pub working_set_bytes: u64,
    /// The predicted memory of the uncovered residents.
    pub uncovered_memory_bytes: u64,
}

/// One host as the replay holds it.
#[derive(Clone, Copy, Debug)]
pub struct HostState {
    /// `C_cpu`, millicores.
    pub cpu_capacity_millis: u64,
    /// `C_mem`, bytes.
    pub memory_capacity_bytes: u64,
    /// The measured rule's pieces at slot 0.
    pub slot0: Slot0,
    /// The sum of the residents' belief charges at slots `1..=30` and `H+`, without the new
    /// lease: entry `i` is slot `i + 1`.
    pub ahead: [Charge; SLOTS - 1],
    /// The `p_bar` of the host's residents that have the lease's key, in millicores.
    pub same_key_millis: u64,
    /// How many residents no full window has covered.
    pub uncovered_leases: u32,
}

/// The left side of the predicate at slot 0, and whether the measurement term won: the
/// measured CPU is strictly above the floor `f * covered`. At a tie the floor is the term
/// reported, the mirror of the shipped count of floor binds, which counts a strictly larger
/// floor.
///
/// The floor is rounded up, as the shipped predicate rounds it.
#[must_use]
pub fn slot0_left(s: &Slot0, floor_permille: u64) -> (Charge, bool) {
    let floor = u64::try_from(
        (u128::from(s.covered_cpu_millis) * u128::from(floor_permille)).div_ceil(1000),
    )
    .unwrap_or(u64::MAX);
    let wins = s.measured_cpu_millis > floor;
    let cpu = s
        .measured_cpu_millis
        .max(floor)
        .saturating_add(s.uncovered_cpu_millis);
    let memory = s
        .memory_all_bytes
        .max(s.working_set_bytes.saturating_add(s.uncovered_memory_bytes));
    (
        Charge {
            cpu_millis: cpu,
            memory_bytes: memory,
        },
        wins,
    )
}

/// A host with its slot 0 worked out once.
#[derive(Clone, Copy, Debug)]
struct Entry {
    state: HostState,
    left0: Charge,
    measurement_wins: bool,
}

/// The replay's state for one placement decision, behind the scorer's seam.
#[derive(Clone, Debug)]
pub struct ScoreModel {
    lease: LeaseBelief,
    hosts: BTreeMap<WorkerId, Entry>,
}

impl ScoreModel {
    /// The model of placing `lease` over `hosts`, with the slot-0 floor `floor_permille`
    /// (see [`FLOOR_PERMILLE`]).
    #[must_use]
    pub fn new(
        lease: LeaseBelief,
        floor_permille: u64,
        hosts: impl IntoIterator<Item = (WorkerId, HostState)>,
    ) -> Self {
        let hosts = hosts
            .into_iter()
            .map(|(name, state)| {
                let (left0, measurement_wins) = slot0_left(&state.slot0, floor_permille);
                (
                    name,
                    Entry {
                        state,
                        left0,
                        measurement_wins,
                    },
                )
            })
            .collect();
        Self { lease, hosts }
    }

    /// Whether the measurement term decided `worker`'s slot-0 CPU.
    #[must_use]
    pub fn measurement_wins(&self, worker: &WorkerId) -> bool {
        self.entry(worker).measurement_wins
    }

    /// A candidate the model does not hold is a bug of the caller, not a state to answer.
    fn entry(&self, worker: &WorkerId) -> &Entry {
        self.hosts
            .get(worker)
            .unwrap_or_else(|| panic!("worker {worker:?} is not in the score model"))
    }
}

impl PredictedLoad for ScoreModel {
    fn lease(&self) -> LeaseFacts {
        LeaseFacts {
            lease: lease_id(self.lease.lease_ix),
            cold: self.lease.cold,
        }
    }

    fn load(&self, worker: &WorkerId, slot: usize) -> Charge {
        let e = self.entry(worker);
        match slot {
            0 => e.left0,
            _ => e.state.ahead[slot - 1],
        }
    }

    fn charge(&self, _worker: &WorkerId, slot: usize) -> Charge {
        self.lease.charge[slot]
    }

    fn host(&self, worker: &WorkerId) -> HostFacts {
        let h = &self.entry(worker).state;
        HostFacts {
            cpu_capacity_millis: h.cpu_capacity_millis,
            memory_capacity_bytes: h.memory_capacity_bytes,
            fit_minutes: self.lease.fit_minutes,
            fallback: false,
            measured_above_predicted: h.slot0.measured_cpu_millis > h.slot0.covered_cpu_millis,
            same_key_millis: h.same_key_millis,
            new_key_millis: self.lease.p_bar_cpu_millis,
            uncovered_leases: h.uncovered_leases,
        }
    }
}

/// What a ranker counted. Integers only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tally {
    /// Calls with at least one candidate.
    pub attempts: u64,
    /// Calls that scored: two or more candidates.
    pub scored_attempts: u64,
    /// Candidates handed to those calls; the scorer evaluates each 64 times.
    pub scored_candidates: u64,
    /// Scored candidates whose slot-0 CPU the measurement decided.
    pub measurement_wins: u64,
}

/// Ranker 5: the score, through [`Scorer::choose`].
#[derive(Clone, Debug, Default)]
pub struct ScoreRanker {
    scorer: Scorer,
    tally: Tally,
}

impl ScoreRanker {
    /// A ranker with the scorer's default flags (balance 1, tolerance 1%, key share 25%,
    /// squared fit): ranker 5.
    #[must_use]
    pub fn new() -> Self {
        Self::with_config(ScoreConfig::default())
    }

    /// A ranker over `config`.
    #[must_use]
    pub fn with_config(config: ScoreConfig) -> Self {
        Self {
            scorer: Scorer::new(config),
            tally: Tally::default(),
        }
    }

    /// The candidate the lease goes to; `None` for no candidate. `candidates` are the hosts
    /// the predicate passed. One candidate is taken and not scored.
    pub fn place<'a>(
        &mut self,
        model: &ScoreModel,
        candidates: &'a [WorkerId],
    ) -> Option<&'a WorkerId> {
        let chosen = self.scorer.choose(model, candidates)?;
        self.tally.attempts += 1;
        if candidates.len() >= 2 {
            self.tally.scored_attempts += 1;
            self.tally.scored_candidates += candidates.len() as u64;
            self.tally.measurement_wins += candidates
                .iter()
                .filter(|w| model.measurement_wins(w))
                .count() as u64;
        }
        Some(chosen)
    }

    /// The counters so far.
    #[must_use]
    pub const fn tally(&self) -> Tally {
        self.tally
    }

    /// The scorer's own count of slot-resource evaluations.
    #[must_use]
    pub const fn evaluations(&self) -> u64 {
        self.scorer.evaluations()
    }
}
