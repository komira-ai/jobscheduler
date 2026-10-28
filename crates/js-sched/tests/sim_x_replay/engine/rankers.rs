//! The compared score rankers of sim x: 3 (area only), 4 (flat fit), 5 (the score) and FW5,
//! with the counters the report and the decides-differently guard read.
//!
//! Rankers 3, 4 and 5 are one code path, [`Scorer::choose_explained`], over one
//! [`ScoreModel`]; the only difference is the scorer's [`FitShape`]:
//!
//! | ranker | fit | what key 5 sees |
//! |---|---|---|
//! | 3 | [`FitShape::Linear`] | the weighted sum of the leftovers, not squared |
//! | 4 | [`FitShape::Flat`] | the squared leftover of the means over the fit slots ("key 5 on window means"); keys 1 to 4 keep the real per-slot figures |
//! | 5 | [`FitShape::Squared`] | the squared leftover at each fit slot |
//! | FW5 | `Squared` | the same as 5 on beliefs that were flattened everywhere, the predicate included |
//!
//! FW5 differs from 5 in what the engine feeds it, not in the scorer: [`RankerKind::flattens_beliefs`]
//! says so, and [`flat_belief`] is the lease's side of it. The hosts' side (the sum of the
//! residents' flat charges) is built by the engine, which holds the residents.
//!
//! [`Ranker`] counts what the report needs and nothing it has to time: attempts, scored
//! attempts, the candidates they scored, scored attempts with `L >= 2`, the rule that decided
//! each placement ([`Decisions`]) and the placement sequence. [`Gd`] turns two rankers'
//! counters into the figures of the decides-differently guard.

use std::collections::BTreeMap;

use js_sched::{Charge, DecidedBy, FitShape, PredictedLoad, SLOTS, ScoreConfig, Scorer};
use js_types::sched::WorkerId;

use super::adapter::{LeaseBelief, ScoreModel};

/// Which compared ranker. Rankers 1 and 2 are not scorer shapes and live elsewhere.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RankerKind {
    /// Ranker 3: area-only best fit.
    Linear,
    /// Ranker 4: flat best fit, key 5 on the means over the fit slots.
    Flat,
    /// Ranker 5: the score.
    Score,
    /// FW5: the score on beliefs flattened everywhere.
    FlatScore,
}

impl RankerKind {
    /// The name the report prints.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Linear => "3 area-only",
            Self::Flat => "4 flat (key 5 on window means)",
            Self::Score => "5 score",
            Self::FlatScore => "FW5 score on flat beliefs",
        }
    }

    /// The fit this ranker's scorer computes.
    #[must_use]
    pub const fn fit(self) -> FitShape {
        match self {
            Self::Linear => FitShape::Linear,
            Self::Flat => FitShape::Flat,
            Self::Score | Self::FlatScore => FitShape::Squared,
        }
    }

    /// Whether the engine hands this ranker beliefs with every curve replaced by its flat
    /// charge, in the predicate and the load vectors as well.
    #[must_use]
    pub const fn flattens_beliefs(self) -> bool {
        matches!(self, Self::FlatScore)
    }

    /// `base` (the flags the run uses) with this ranker's fit.
    #[must_use]
    pub const fn config(self, base: ScoreConfig) -> ScoreConfig {
        base.with_fit(self.fit())
    }
}

/// The lease's belief with every slot at `flat` and `L = 1`: what a flat predicate would
/// charge it. The cold flag, the index and `p_bar` are kept.
#[must_use]
pub fn flat_belief(lease: &LeaseBelief, flat: Charge) -> LeaseBelief {
    LeaseBelief {
        fit_minutes: 1,
        charge: [flat; SLOTS],
        ..*lease
    }
}

/// How many placements each rule decided. The rules are the scorer's [`DecidedBy`], so the
/// counts cover every case once and their sum is the number of placements.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Decisions {
    by: BTreeMap<DecidedBy, u64>,
}

impl Decisions {
    fn record(&mut self, by: DecidedBy) {
        *self.by.entry(by).or_insert(0) += 1;
    }

    /// The placements `by` decided.
    #[must_use]
    pub fn count(&self, by: DecidedBy) -> u64 {
        self.by.get(&by).copied().unwrap_or(0)
    }

    /// Every placement.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.by.values().sum()
    }

    /// The share of placements `by` decided, in parts per million, rounded down; 0 when
    /// nothing was placed.
    #[must_use]
    pub fn share_ppm(&self, by: DecidedBy) -> u64 {
        ratio_ppm(self.count(by), self.total())
    }

    /// Every rule that decided at least one placement, in the scorer's order, with its count.
    pub fn iter(&self) -> impl Iterator<Item = (DecidedBy, u64)> + '_ {
        self.by.iter().map(|(by, n)| (*by, *n))
    }
}

/// `part / whole` in parts per million, rounded down; 0 for an empty whole.
#[must_use]
pub fn ratio_ppm(part: u64, whole: u64) -> u64 {
    if whole == 0 {
        return 0;
    }
    u64::try_from(u128::from(part) * 1_000_000 / u128::from(whole)).unwrap_or(u64::MAX)
}

/// Parts per million as a percentage with four decimals, in integers: `250000` is `25.0000%`.
#[must_use]
pub fn percent(ppm: u64) -> String {
    format!("{}.{:04}%", ppm / 10_000, ppm % 10_000)
}

/// A score ranker: the scorer under one fit, with its counters.
#[derive(Clone, Debug)]
pub struct Ranker {
    kind: RankerKind,
    scorer: Scorer,
    attempts: u64,
    scored: u64,
    scored_candidates: u64,
    scored_long: u64,
    decisions: Decisions,
    placements: Vec<(u64, WorkerId)>,
}

impl Ranker {
    /// A ranker of `kind` over the flags `base`.
    #[must_use]
    pub fn new(kind: RankerKind, base: ScoreConfig) -> Self {
        Self {
            kind,
            scorer: Scorer::new(kind.config(base)),
            attempts: 0,
            scored: 0,
            scored_candidates: 0,
            scored_long: 0,
            decisions: Decisions::default(),
            placements: Vec::new(),
        }
    }

    /// Which ranker this is.
    #[must_use]
    pub const fn kind(&self) -> RankerKind {
        self.kind
    }

    /// The candidate the lease goes to; `None` for no candidate. `candidates` are the hosts
    /// the predicate passed, in any order: the scorer's order is total, so the order handed
    /// in does not matter.
    pub fn place<'a>(
        &mut self,
        model: &ScoreModel,
        candidates: &'a [WorkerId],
    ) -> Option<&'a WorkerId> {
        let (chosen, by) = self.scorer.choose_explained(model, candidates)?;
        self.attempts += 1;
        self.decisions.record(by);
        if by != DecidedBy::Only {
            self.scored += 1;
            self.scored_candidates += candidates.len() as u64;
            if candidates
                .first()
                .is_some_and(|w| model.host(w).fit_minutes >= 2)
            {
                self.scored_long += 1;
            }
        }
        self.placements
            .push((model.lease().lease.seq, chosen.clone()));
        Some(chosen)
    }

    /// Calls with at least one candidate: the placements.
    #[must_use]
    pub const fn attempts(&self) -> u64 {
        self.attempts
    }

    /// Placements that were scored: two or more candidates.
    #[must_use]
    pub const fn scored(&self) -> u64 {
        self.scored
    }

    /// Candidates handed to the scored placements; the scorer evaluates each 64 times.
    #[must_use]
    pub const fn scored_candidates(&self) -> u64 {
        self.scored_candidates
    }

    /// Scored placements of a lease with `L >= 2`: the ones a curve can change.
    #[must_use]
    pub const fn scored_long(&self) -> u64 {
        self.scored_long
    }

    /// The scorer's own count of slot-resource evaluations.
    #[must_use]
    pub const fn evaluations(&self) -> u64 {
        self.scorer.evaluations()
    }

    /// The rule that decided each placement.
    #[must_use]
    pub const fn decisions(&self) -> &Decisions {
        &self.decisions
    }

    /// The placements in order: the lease's index and the host it went to.
    #[must_use]
    pub fn placements(&self) -> &[(u64, WorkerId)] {
        &self.placements
    }

    /// The hash of the placement sequence, see [`sequence_hash`].
    #[must_use]
    pub fn sequence_hash(&self) -> u64 {
        sequence_hash(&self.placements)
    }
}

/// FNV-1a over the placements in order: each lease index as eight little-endian bytes, then
/// the host's name preceded by its length as a `u32` (the fixed-width index already marks
/// where a name ends; the length keeps the encoding self-delimiting on its own). Two runs with the same placements in
/// the same order have the same hash in every process.
#[must_use]
pub fn sequence_hash(placements: &[(u64, WorkerId)]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    let mut eat = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    };
    for (lease_ix, worker) in placements {
        eat(&lease_ix.to_le_bytes());
        let name = worker.as_str().as_bytes();
        eat(&u32::try_from(name.len()).unwrap_or(u32::MAX).to_le_bytes());
        eat(name);
    }
    hash
}

/// How many of `a`'s placements went to a different host in `b`, matched by lease index. A
/// lease `b` did not place counts as different.
#[must_use]
pub fn differing(a: &[(u64, WorkerId)], b: &[(u64, WorkerId)]) -> u64 {
    let other: BTreeMap<u64, &WorkerId> = b.iter().map(|(ix, w)| (*ix, w)).collect();
    a.iter()
        .filter(|(ix, w)| other.get(ix).is_none_or(|o| *o != w))
        .count() as u64
}

/// The figures of the decides-differently guard for ranker `x` against ranker `y`: the
/// guard says whether a comparison of the two can show a difference at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gd {
    /// `x`'s placements.
    pub placements: u64,
    /// `x`'s scored placements (two or more candidates).
    pub scored: u64,
    /// Scored placements with `L >= 2`.
    pub scored_long: u64,
    /// Placements whose host differs in `y`.
    pub differ: u64,
}

/// The guard's floors: scored placements at least this share of placements and this many.
pub const GD_SCORED_PPM: u64 = 200_000;
/// See [`GD_SCORED_PPM`].
pub const GD_SCORED_MIN: u64 = 2_000;
/// Scored placements with `L >= 2`, at least this share of placements.
pub const GD_LONG_PPM: u64 = 50_000;

impl Gd {
    /// The figures of `x` against `y`.
    #[must_use]
    pub fn of(x: &Ranker, y: &Ranker) -> Self {
        Self {
            placements: x.attempts(),
            scored: x.scored(),
            scored_long: x.scored_long(),
            differ: differing(x.placements(), y.placements()),
        }
    }

    /// `Ok` when every floor holds; otherwise the name of the first that does not, for
    /// `UNINFORMATIVE: <guard>`. `min_differ_ppm` is the share of placements that must
    /// differ.
    ///
    /// # Errors
    ///
    /// The failed guard.
    pub fn check(&self, min_differ_ppm: u64) -> Result<(), &'static str> {
        if ratio_ppm(self.scored, self.placements) < GD_SCORED_PPM {
            Err("scored placements below the share of placements")
        } else if self.scored < GD_SCORED_MIN {
            Err("too few scored placements")
        } else if ratio_ppm(self.scored_long, self.placements) < GD_LONG_PPM {
            Err("scored placements with L >= 2 below the share of placements")
        } else if ratio_ppm(self.differ, self.placements) < min_differ_ppm {
            Err("placement sequences differ in too few placements")
        } else {
            Ok(())
        }
    }

    /// One report line.
    #[must_use]
    pub fn line(&self, label: &str) -> String {
        format!(
            "GD {label}: placements {} scored {} ({}) scored L>=2 {} ({}) differ {} ({})",
            self.placements,
            self.scored,
            percent(ratio_ppm(self.scored, self.placements)),
            self.scored_long,
            percent(ratio_ppm(self.scored_long, self.placements)),
            self.differ,
            percent(ratio_ppm(self.differ, self.placements)),
        )
    }
}
