//! The score rankers (3, 4, 5 and FW5 of [`super::rankers`]) as rules of the replay loop.
//!
//! [`ScoreRule`] is a [`Ranker`] of [`super::rules`]: at each placement it builds the
//! scorer's [`ScoreModel`] from the replay's own state and hands it to a
//! [`rankers::Ranker`]. What each host figure is:
//!
//! | scorer figure | from the replay's host |
//! |---|---|
//! | capacities | its [`Capacity`] (memory in bytes) |
//! | slot 0: measured CPU | [`HostState::measured_cpu_millis`] |
//! | slot 0: covered, uncovered CPU and memory | the flat charges of [`HostState::sums`] |
//! | slot 0: all memory | `sums.slots[0]` |
//! | slot 0: working set | [`HostState::working_set_mib`] |
//! | slots 1 to 31 | `sums.slots[1..]` |
//! | same-key share | the flat CPU of the residents of the lease's key |
//! | uncovered leases | the residents placed in this round |
//!
//! and for the lease its belief's charge at every slot, clamped to the host as the
//! predicate clamps it, `L` and its flat CPU. Slot 0 is worked out by
//! [`super::adapter::slot0_left`] with the predicate's `f`, so the scorer's slot 0 is the
//! predicate's left side. FW5 is the score ranker over flattened beliefs: what the engine
//! feeds it, not a rule of its own (see [`super::x2::flatten`]).
//!
//! A host is named `h000`, `h001`, ... by its index, so name order is index order and the
//! scorer's tie-break over names is the same in every run.

use js_sched::{Charge as Sched, DecidedBy, SLOTS, ScoreConfig};
use js_types::sched::WorkerId;

use super::adapter::{HostState as ScoreHost, LeaseBelief, ScoreModel, Slot0};
use super::fixture::KeyIx;
use super::metrics::Decided;
use super::model::{Capacity, Charge, HostState};
use super::rankers::{self, RankerKind};
use super::rules::{Choose, Pick, Ranker};

const MIB: u64 = 1 << 20;

/// A replay charge in the scorer's units (memory in bytes).
pub fn sched_charge(c: Charge) -> Sched {
    Sched {
        cpu_millis: c.cpu_millis,
        memory_bytes: c.mem_mib.saturating_mul(MIB),
    }
}

/// The replay's name of a scorer decision.
pub fn decided(by: DecidedBy) -> Decided {
    match by {
        DecidedBy::Key(k) => Decided::Key(k),
        DecidedBy::PackLimit => Decided::PackLimit,
        DecidedBy::TieBreak(t) => Decided::TieBreak(t),
        DecidedBy::Only => Decided::Only,
    }
}

/// The name of host `index`.
pub fn host_name(index: usize) -> WorkerId {
    WorkerId::new(format!("h{index:03}"))
}

/// The scorer's view of host `host` for a lease of `key`: see the module's table.
pub fn score_host(host: &HostState<'_>, key: KeyIx, keys: &[KeyIx]) -> ScoreHost {
    let cap: &Capacity = &host.cap;
    let sums = host.sums();
    let mut ahead = [Sched::default(); SLOTS - 1];
    for (slot, out) in ahead.iter_mut().enumerate() {
        *out = sched_charge(sums.slots[slot + 1]);
    }
    let same_key_millis = host
        .residents()
        .iter()
        .filter(|(lease, _)| keys[**lease] == key)
        .map(|(_, r)| r.belief.p_bar.clamped(cap).cpu_millis)
        .sum();
    let uncovered_leases = host
        .residents()
        .values()
        .filter(|r| r.start_tick == host.last_round())
        .count();
    ScoreHost {
        cpu_capacity_millis: cap.cpu_millis,
        memory_capacity_bytes: cap.mem_mib.saturating_mul(MIB),
        slot0: Slot0 {
            measured_cpu_millis: host.measured_cpu_millis(),
            covered_cpu_millis: sums.covered.cpu_millis,
            uncovered_cpu_millis: sums.uncovered.cpu_millis,
            memory_all_bytes: sums.slots[0].mem_mib.saturating_mul(MIB),
            working_set_bytes: host.working_set_mib().saturating_mul(MIB),
            uncovered_memory_bytes: sums.uncovered.mem_mib.saturating_mul(MIB),
        },
        ahead,
        same_key_millis,
        uncovered_leases: u32::try_from(uncovered_leases).unwrap_or(u32::MAX),
    }
}

/// The scorer's view of the lease of `c`, its charges clamped to a host of `cap`.
pub fn score_lease(c: &Choose<'_, '_>, cap: &Capacity) -> LeaseBelief {
    let mut charge = [Sched::default(); SLOTS];
    for (slot, out) in charge.iter_mut().enumerate() {
        *out = sched_charge(c.belief.at_slot(0, slot).clamped(cap));
    }
    LeaseBelief {
        lease_ix: c.lease as u64,
        cold: c.belief.cold,
        fit_minutes: c.belief.fit_minutes,
        p_bar_cpu_millis: c.belief.p_bar.clamped(cap).cpu_millis,
        charge,
    }
}

/// A score ranker in the replay loop.
#[derive(Clone, Debug)]
pub struct ScoreRule {
    ranker: rankers::Ranker,
    keys: Vec<KeyIx>,
}

impl ScoreRule {
    /// The ranker `kind` over the flags `base`, for leases whose keys are `keys` (lease `i`
    /// has key `keys[i]`).
    pub fn new(kind: RankerKind, base: ScoreConfig, keys: Vec<KeyIx>) -> Self {
        Self {
            ranker: rankers::Ranker::new(kind, base),
            keys,
        }
    }

    /// The score ranker and what it counted.
    pub fn ranker(&self) -> &rankers::Ranker {
        &self.ranker
    }
}

impl Ranker for ScoreRule {
    fn choose(&mut self, c: &Choose<'_, '_>) -> Pick {
        let cap = c.hosts[c.candidates[0]].cap;
        let key = self.keys[c.lease];
        let model = ScoreModel::new(
            score_lease(c, &cap),
            c.params.f_permille,
            c.candidates
                .iter()
                .map(|&h| (host_name(h), score_host(&c.hosts[h], key, &self.keys))),
        );
        let names: Vec<WorkerId> = c.candidates.iter().map(|&h| host_name(h)).collect();
        let before = self.ranker.evaluations();
        let (chosen, by) = self
            .ranker
            .place_explained(&model, &names)
            .expect("the replay hands a ranker at least one candidate");
        let at = names
            .iter()
            .position(|n| n == chosen)
            .expect("the scorer chooses one of the candidates");
        Pick {
            host: c.candidates[at],
            decided: decided(by),
            scored_candidates: if by == DecidedBy::Only {
                0
            } else {
                names.len() as u64
            },
            evaluations: self.ranker.evaluations() - before,
        }
    }
}
