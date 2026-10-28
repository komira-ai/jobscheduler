//! The control's permutation of belief minutes, and the share of the beliefs it moves.
//!
//! The control asks whether the gain of a curve-aware score over the area-only score comes
//! from the shape of the curves. It answers by shuffling the minutes of every key's belief
//! curve and replaying again: if the shape is what pays, the gain must go.
//!
//! # The permutation
//!
//! A warm belief with life `L` has its minutes `0..L` permuted by one permutation that
//! moves the cores curve and the memory curve together (minute `t` of the new belief is
//! minute `perm[t]` of the old one, for both), so a minute keeps its pair of values.
//!
//! * It is a function of `(seed, key, L)` alone: the Fisher-Yates swap at position `i` is
//!   `mix(seed, DOMAIN_PERM, key, i) % (i + 1)`. Four seeds are four draws, and no other
//!   draw of the replay is shifted by it.
//! * It is a bijection on `0..L`, so the multiset of values (and so the area) is kept, and
//!   the identity when `L <= 1`.
//! * Minutes from `L` on, the held tail and the `H+` slot read, are not touched, and
//!   neither are the flag, `L` and `p_bar`. A curve shorter than `L + 1` points is first
//!   extended by holding its last point, so a read past `L` returns what it returned
//!   before.
//!
//! # The movable share
//!
//! A permutation can leave a curve as it was (a flat curve, a short life, a lucky draw).
//! [`movable`] counts how much of the beliefs it really moves, so a control that shuffles
//! nothing is seen as such. A minute of a lease is moved if the pair (cores, memory) read at
//! it differs after the permutation. Core-minutes are the cores of the warm leases' minutes
//! `0..L` (a cold belief has `L = 1` and a flat guess, and is left out of them); the leases
//! are all the leases.

use super::fixture::{KeyIx, Row};
use super::predictor::{Belief, Predictor, Tail};
use super::truth::{DOMAIN_PERM, mix};

/// The share of core-minutes (parts per million) the permutation must move for the control
/// to be a control, as measured on the committed trace.
///
/// Measured: the committed trace (145,379 leases, 97,765 of them warm), the learner on the
/// trace clock, truth seed 1 with the default jittered shapes and permutation seed 1 move
/// 528,604 ppm of the warm leases' predicted core-minutes (219,667 ppm of the leases); seeds
/// 2 to 4 give 524,896, 516,140 and 540,010. A minute counts as moved when its cores or its
/// memory change, and the memory curve ramps over a run's first minutes, so a share of this
/// size does not by itself mean the CPU shape is what moves (by cores alone, seed 1:
/// 421,568 ppm; the test prints both). The rule: the floor is the measured
/// share, seed 1, halved and rounded down, so a later change that halves what the control
/// moves fails a test; a measured share under 100,000 ppm would mean the control cannot
/// assert and only reports.
pub const DID_ANYTHING_FLOOR_PPM: u64 = 264_302;

/// Source minutes of the permutation for `(seed, key)` over `0..life`: the new belief at
/// minute `t` is the old one at `perm[t]`. Empty for `life == 0`.
pub fn permutation(seed: u64, key: KeyIx, life: usize) -> Vec<usize> {
    let mut perm: Vec<usize> = (0..life).collect();
    for i in (1..life).rev() {
        let draw = mix(seed, DOMAIN_PERM, u64::from(key.0), i as u64);
        // `i + 1` is at most `life`, so the remainder fits a `usize`.
        perm.swap(i, (draw % (i as u64 + 1)) as usize);
    }
    perm
}

/// The belief with its minutes `0..L` permuted, cores and memory jointly, as the module
/// comment says. A cold belief, or a warm one with `L <= 1`, is returned as it is.
pub fn permute_belief(belief: &Belief, seed: u64, key: KeyIx) -> Belief {
    let life = usize::from(belief.life_minutes());
    if life <= 1 {
        return belief.clone();
    }
    let perm = permutation(seed, key, life);
    let (cores, mem) = belief.curves();
    let points = cores.len().max(life + 1);
    let held = |curve: &[u64], t: usize| curve[t.min(curve.len() - 1)];
    let moved = |curve: &[u64]| -> Vec<u64> {
        (0..points)
            .map(|t| held(curve, perm.get(t).copied().unwrap_or(t)))
            .collect()
    };
    belief
        .with_curves(moved(cores), moved(mem))
        .expect("two curves of one length, not empty")
}

/// What the permutation moves, summed over leases.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Movable {
    /// Leases.
    pub leases: u64,
    /// Leases with a warm belief.
    pub warm: u64,
    /// Leases whose believed curve changes under the permutation.
    pub moved: u64,
    /// Millicore-minutes of the warm leases' minutes `0..L`.
    pub core_milli_minutes: u128,
    /// Of those, the millicore-minutes at minutes whose (cores, memory) pair changes.
    pub moved_core_milli_minutes: u128,
    /// Of those, the millicore-minutes at minutes whose cores alone change (reported: the
    /// memory curve ramps over a run's first minutes, so it moves with no CPU shape).
    pub cores_moved_core_milli_minutes: u128,
}

impl Movable {
    /// The share of leases that move, in parts per million of all leases; 0 for none.
    pub fn lease_ppm(&self) -> u64 {
        ppm(u128::from(self.moved), u128::from(self.leases))
    }

    /// The share of warm core-minutes whose cores change, in parts per million; 0 for none.
    pub fn cores_only_ppm(&self) -> u64 {
        ppm(self.cores_moved_core_milli_minutes, self.core_milli_minutes)
    }

    /// The share of warm core-minutes that move, in parts per million; 0 for none.
    pub fn core_minute_ppm(&self) -> u64 {
        ppm(self.moved_core_milli_minutes, self.core_milli_minutes)
    }
}

fn ppm(part: u128, whole: u128) -> u64 {
    if whole == 0 {
        return 0;
    }
    u64::try_from(part * 1_000_000 / whole).unwrap_or(u64::MAX)
}

/// One lease's weights, millicore-minutes.
#[derive(Default)]
struct Weighed {
    all: u128,
    moved: u128,
    cores_moved: u128,
    any: bool,
}

/// The cores of the minutes `0..L` of `belief` and of those the permutation `perm` of the
/// same length changes, as a [`Weighed`].
fn weigh(belief: &Belief, perm: &[usize]) -> Weighed {
    let mut w = Weighed::default();
    for (t, &source) in perm.iter().enumerate() {
        let cores = belief.cores_milli_at(t, Tail::Held);
        w.all += u128::from(cores);
        let cores_changed = cores != belief.cores_milli_at(source, Tail::Held);
        let mem_changed = belief.mem_mib_at(t, Tail::Held) != belief.mem_mib_at(source, Tail::Held);
        if cores_changed {
            w.cores_moved += u128::from(cores);
        }
        if cores_changed || mem_changed {
            w.moved += u128::from(cores);
            w.any = true;
        }
    }
    w
}

/// What permutation seed `seed` moves in the beliefs `predictor` holds for `rows` (row `i`
/// is lease `i`).
///
/// # Panics
/// If the predictor was not built from `rows`.
pub fn movable(rows: &[Row], predictor: &Predictor, seed: u64) -> Movable {
    assert_eq!(rows.len(), predictor.len(), "one belief per row");
    let mut total = Movable::default();
    for (i, row) in rows.iter().enumerate() {
        let belief = predictor.belief(i);
        total.leases += 1;
        if belief.is_cold() {
            continue;
        }
        total.warm += 1;
        let life = usize::from(belief.life_minutes());
        let perm = permutation(seed, row.key, life);
        let w = weigh(belief, &perm);
        total.core_milli_minutes += w.all;
        total.moved_core_milli_minutes += w.moved;
        total.cores_moved_core_milli_minutes += w.cores_moved;
        if w.any {
            total.moved += 1;
        }
    }
    total
}
