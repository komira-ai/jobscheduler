//! How a lease is charged over time: a charge per minute of the horizon instead of one
//! number for its whole life (the time-profile layer of the placement-by-load design).
//!
//! The predicate looks at `tau` minutes ahead for `tau` in `0..=HORIZON`, and once more at
//! the slot `H_PLUS` (what each lease asks for from minute `HORIZON` on). A lease of age `e`
//! minutes is charged, at slot `tau`:
//!
//! ```text
//! CPU, by curve:  c(tau) = max(curve(e + tau), g * p_bar)
//! CPU, flat:      p_bar          (covered leases: f * p_bar, applied to the sum)
//! memory, curve:  R(tau)  = max(remaining maximum of the curve from e + tau, held)
//! memory, flat:   m
//! ```
//!
//! A lease that has just started (no reading covers it yet) is charged, at its first two
//! minutes, the larger of the curve's minutes 0 and 1, so it is never counted at a quiet
//! start. `held` is the working set of the lease's newest closed minute when that was
//! above the curve's remaining maximum from that minute: a lease past its curve is charged
//! what it uses, until a minute says it uses less.
//!
//! A lease is charged flat, as before, when its key is cold, has fewer than
//! [`MIN_RUNS`] runs, is demoted by the shape guard, is a retry up the memory ladder, or
//! sits on a worker charged by prediction alone; a flat lease is a lane of constants.
//!
//! The sums of the charges of the leases on one worker are kept per slot ([`Lanes`]) and
//! rebuilt whenever a reading moves a lease's age; a placement adds one lease's.

use std::sync::Arc;

use js_types::sched::usage::{Curve, LeaseCost};

use super::ledger::Entry;
use super::scale_up;

/// `H`: how many minutes ahead the predicate looks.
pub const HORIZON_MINUTES: u32 = 30;
pub(crate) const HORIZON: u32 = HORIZON_MINUTES;

/// The slot of the `H+` check: what each lease asks for from minute `HORIZON` on.
pub(crate) const H_PLUS: usize = 31;

/// The slots checked: minutes `0..=HORIZON`, then `H_PLUS`.
pub(crate) const SLOTS: usize = 32;

/// A key is charged by curve once its curve was fitted from this many runs.
pub(crate) const MIN_RUNS: u16 = 3;

/// The first minutes of an uncovered lease's life that are charged at the larger of the
/// curve's minutes 0 and 1.
const IN_FLIGHT_MINUTES: u32 = 2;

/// The minutes ahead a slot stands for.
pub(crate) fn slot_minute(slot: usize) -> u32 {
    u32::try_from(slot).map_or(HORIZON, |s| s.min(HORIZON))
}

/// A sum per slot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Lane([u64; SLOTS]);

impl Lane {
    pub(crate) const fn at(&self, slot: usize) -> u64 {
        self.0[slot]
    }

    fn shift(&mut self, slot: usize, value: u64, up: bool) {
        let cell = &mut self.0[slot];
        *cell = if up {
            cell.saturating_add(value)
        } else {
            cell.saturating_sub(value)
        };
    }
}

fn shift(cell: &mut u64, value: u64, up: bool) {
    *cell = if up {
        cell.saturating_add(value)
    } else {
        cell.saturating_sub(value)
    };
}

/// The charges, per slot, of the leases on a worker that are charged over time: by curve,
/// or flat but ending at an expected life. The flat charge of every other lease stays in
/// the scalar sums of the ledger, and the `timed_*` figures are what to take off those
/// sums for the leases counted here.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Lanes {
    /// Covered leases charged flat for a while: their predicted cores (the floor `f`
    /// applies to the sum).
    pub(crate) cpu_cov_p: Lane,
    /// Covered leases charged by curve.
    pub(crate) cpu_cov_c: Lane,
    /// Uncovered leases of both kinds.
    pub(crate) cpu_unc: Lane,
    /// Memory of every lease counted here.
    pub(crate) mem_all: Lane,
    /// Of those, the uncovered.
    pub(crate) mem_unc: Lane,
    /// The flat cores of the covered / uncovered leases counted here, as the scalar sums
    /// hold them.
    pub(crate) timed_cpu_cov_p: u64,
    pub(crate) timed_cpu_unc_p: u64,
    /// The flat memory of the covered / uncovered leases counted here.
    pub(crate) timed_mem_cov: u64,
    pub(crate) timed_mem_unc: u64,
    /// How many leases are counted here (CPU or memory).
    pub(crate) timed: usize,
}

impl Lanes {
    /// Adds (`up`) or takes off the charges of `e`.
    pub(super) fn shift(&mut self, e: &Entry, up: bool) {
        let Some(p) = e.profile.as_deref().filter(|_| !e.vm) else {
            return;
        };
        let cpu_curve = p.cpu_by_curve() && e.aged;
        let mem_curve = p.mem_by_curve() && e.aged;
        let cpu_timed = cpu_curve || p.life.is_some();
        let mem_timed = mem_curve || p.life.is_some();
        if cpu_timed || mem_timed {
            self.timed = if up {
                self.timed + 1
            } else {
                self.timed.saturating_sub(1)
            };
        }
        if cpu_timed {
            for slot in 0..SLOTS {
                let v = p.cpu_charge(e.cost.cpu_millis, e.covered, cpu_curve, e.age, slot);
                match (e.covered, cpu_curve) {
                    (true, true) => self.cpu_cov_c.shift(slot, v, up),
                    (true, false) => self.cpu_cov_p.shift(slot, v, up),
                    (false, _) => self.cpu_unc.shift(slot, v, up),
                }
            }
            let sum = if e.covered {
                &mut self.timed_cpu_cov_p
            } else {
                &mut self.timed_cpu_unc_p
            };
            shift(sum, e.cost.cpu_millis, up);
        }
        if mem_timed {
            for slot in 0..SLOTS {
                let v = p.mem_charge(e.cost.memory_bytes, mem_curve, e.age, slot, e.ws_over);
                self.mem_all.shift(slot, v, up);
                if !e.covered {
                    self.mem_unc.shift(slot, v, up);
                }
            }
            let sum = if e.covered {
                &mut self.timed_mem_cov
            } else {
                &mut self.timed_mem_unc
            };
            shift(sum, e.cost.memory_bytes, up);
        }
    }
}

/// What is known about how a lease is charged over time: its key's curve, which resources
/// use it, and when its charge ends. Fixed when the lease is placed, and kept on the
/// worker's ledger until the lease ends.
#[derive(Clone, Debug)]
pub(crate) struct Profile {
    curve: Option<Arc<Curve>>,
    /// CPU is charged by the curve (the setting is on and the shape guard has not
    /// demoted the key).
    cpu: bool,
    /// Memory is charged by the curve.
    mem: bool,
    /// The shape guard has the key's CPU charged flat.
    pub(crate) demoted_cpu: bool,
    /// The shape guard has the key's memory charged flat.
    pub(crate) demoted_mem: bool,
    /// `g * p_bar`: the least a curve charges.
    cpu_floor: u64,
    /// Minutes after which the charge ends, when ending it at the expected life is on.
    life: Option<u32>,
    /// `p_bar`: the flat cores.
    p_bar: u64,
    /// The flat memory.
    m_flat: u64,
    /// The key the shape guard judges the lease's runs under.
    pub(crate) guard_key: u64,
    /// What the lease would be charged at each slot when it starts now.
    new_cpu: Lane,
    new_mem: Lane,
}

/// The choices that make a [`Profile`].
pub(crate) struct Choice {
    pub(crate) curve: Option<Curve>,
    pub(crate) cpu: bool,
    pub(crate) mem: bool,
    pub(crate) demoted_cpu: bool,
    pub(crate) demoted_mem: bool,
    pub(crate) life: Option<u32>,
    pub(crate) floor_permille: u32,
    pub(crate) guard_key: u64,
}

impl Profile {
    /// The profile of a lease of `cost`.
    pub(crate) fn new(choice: Choice, cost: &LeaseCost) -> Self {
        let mut p = Self {
            curve: choice.curve.map(Arc::new),
            cpu: choice.cpu,
            mem: choice.mem,
            demoted_cpu: choice.demoted_cpu,
            demoted_mem: choice.demoted_mem,
            cpu_floor: scale_up(cost.cpu_millis, u64::from(choice.floor_permille), 1000),
            life: choice.life,
            p_bar: cost.cpu_millis,
            m_flat: cost.memory_bytes,
            guard_key: choice.guard_key,
            new_cpu: Lane::default(),
            new_mem: Lane::default(),
        };
        for slot in 0..SLOTS {
            p.new_cpu.0[slot] = p.cpu_charge(p.p_bar, false, p.cpu_by_curve(), 0, slot);
            p.new_mem.0[slot] = p.mem_charge(p.m_flat, p.mem_by_curve(), 0, slot, 0);
        }
        p
    }

    /// CPU is charged by the curve.
    pub(crate) fn cpu_by_curve(&self) -> bool {
        self.cpu && self.curve.is_some()
    }

    /// Memory is charged by the curve.
    pub(crate) fn mem_by_curve(&self) -> bool {
        self.mem && self.curve.is_some()
    }

    /// Whether the lease is charged by its key's curve in CPU or in memory.
    pub(crate) fn by_curve(&self) -> bool {
        self.cpu_by_curve() || self.mem_by_curve()
    }

    /// Whether the lease's charge is a lane over time, not a constant, in some resource.
    pub(crate) fn timed(&self) -> bool {
        self.cpu_by_curve() || self.mem_by_curve() || self.life.is_some()
    }

    /// What a new lease of this profile is charged in CPU at `slot`.
    pub(crate) const fn new_cpu(&self, slot: usize) -> u64 {
        self.new_cpu.at(slot)
    }

    /// What a new lease of this profile is charged in memory at `slot`.
    pub(crate) const fn new_mem(&self, slot: usize) -> u64 {
        self.new_mem.at(slot)
    }

    /// Whether the charge is still on `tau` minutes ahead, for a lease of age `age`. The
    /// present minute is always charged.
    fn alive(&self, age: u32, tau: u32) -> bool {
        tau == 0 || self.life.is_none_or(|life| age.saturating_add(tau) < life)
    }

    fn cores_at(&self, minute: u32) -> u64 {
        self.curve
            .as_deref()
            .and_then(|c| c.at(minute))
            .map_or(0, |p| u64::from(p.cores_milli))
    }

    /// The CPU charge, in millicores, of a lease of age `age` minutes at `slot`.
    ///
    /// By curve: the curve at minute `age + tau` (an uncovered lease in its first two
    /// minutes: the larger of the curve's minutes 0 and 1; the `H+` slot: the largest
    /// from `age + HORIZON` on), never below the floor. Otherwise `p_flat` (the caller
    /// applies the covered floor `f` to the sum of these). Zero once the expected life is
    /// over.
    pub(crate) fn cpu_charge(
        &self,
        p_flat: u64,
        covered: bool,
        by_curve: bool,
        age: u32,
        slot: usize,
    ) -> u64 {
        let tau = slot_minute(slot);
        if !self.alive(age, tau) {
            return 0;
        }
        if !by_curve {
            return p_flat;
        }
        let minute = age.saturating_add(tau);
        let raw = if slot == H_PLUS {
            self.curve
                .as_deref()
                .map_or(0, |c| u64::from(c.remaining_max_cores(minute)))
        } else if !covered && minute < IN_FLIGHT_MINUTES {
            self.cores_at(0).max(self.cores_at(1))
        } else {
            self.cores_at(minute)
        };
        raw.max(self.cpu_floor)
    }

    /// The memory charge, in bytes, of a lease of age `age` minutes at `slot`: the largest
    /// working set the curve asks for from minute `age + tau` on, never below `held`.
    /// Otherwise `m_flat`. Zero once the expected life is over.
    pub(crate) fn mem_charge(
        &self,
        m_flat: u64,
        by_curve: bool,
        age: u32,
        slot: usize,
        held: u64,
    ) -> u64 {
        let tau = slot_minute(slot);
        if !self.alive(age, tau) {
            return 0;
        }
        if !by_curve {
            return m_flat;
        }
        let remaining = self
            .curve
            .as_deref()
            .map_or(0, |c| c.remaining_max_ws(age.saturating_add(tau)));
        remaining.max(held)
    }

    /// The largest working set the curve asks for from `minute` on.
    pub(crate) fn mem_remaining(&self, minute: u32) -> u64 {
        self.curve
            .as_deref()
            .map_or(0, |c| c.remaining_max_ws(minute))
    }

    /// Whether a lease that used `mean_millis` cores over its minute `minute` misfit its
    /// CPU curve: more than twice its charge, and by at least 250 m (so that a tiny
    /// charge cannot dominate).
    pub(crate) fn cpu_misfit(&self, minute: u32, mean_millis: u32) -> bool {
        if !self.cpu_by_curve() {
            return false;
        }
        let charge = self.cpu_charge(self.p_bar, true, true, minute, 0);
        let actual = u64::from(mean_millis);
        actual > charge.saturating_mul(2) && actual >= charge.saturating_add(250)
    }

    /// Whether a lease whose working set reached `ws_max` bytes in its minute `minute`
    /// misfit its memory curve: above the curve's remaining maximum.
    pub(crate) fn mem_misfit(&self, minute: u32, ws_max: u64) -> bool {
        self.mem_by_curve() && ws_max > self.mem_remaining(minute)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use js_types::sched::usage::MinutePoint;

    const GIB: u64 = 1 << 30;

    fn curve(points: &[(u32, u64)]) -> Curve {
        let points = points
            .iter()
            .map(|&(cores_milli, working_set_bytes)| MinutePoint {
                cores_milli,
                working_set_bytes,
            })
            .collect();
        Curve::new(points, 5).expect("short curve")
    }

    fn cost(cpu: u64, mem: u64) -> LeaseCost {
        LeaseCost {
            cpu_millis: cpu,
            memory_bytes: mem,
            limit_bytes: mem,
            cold: false,
            retry: false,
            key_hash: 7,
        }
    }

    fn profile(points: &[(u32, u64)], cpu: u64, mem: u64) -> Profile {
        Profile::new(
            Choice {
                curve: Some(curve(points)),
                cpu: true,
                mem: true,
                demoted_cpu: false,
                demoted_mem: false,
                life: None,
                floor_permille: 750,
                guard_key: 7,
            },
            &cost(cpu, mem),
        )
    }

    /// Catches: the floor missing (a quiet minute charged below `g * p_bar`), the curve
    /// not read at `age + tau`, a lease past the curve not charged the tail, and the `H+`
    /// slot taking the point instead of the largest from minute `HORIZON` on.
    #[test]
    fn cpu_is_the_curve_above_the_floor_at_the_minute_ahead() {
        // p_bar 10 cores, floor 7.5 cores; busy 40 cores at minutes 2 and 40.
        let mut points = vec![(1_000, 0); 42];
        points[2] = (40_000, 0);
        points[40] = (40_000, 0);
        let p = profile(&points, 10_000, GIB);
        let at = |covered, age, slot| p.cpu_charge(10_000, covered, true, age, slot);
        assert_eq!(at(true, 0, 0), 7_500, "quiet minute is floored");
        assert_eq!(at(true, 0, 2), 40_000);
        assert_eq!(at(true, 1, 1), 40_000, "age plus tau");
        assert_eq!(at(true, 5, 0), 7_500);
        assert_eq!(at(true, 0, 30), 7_500, "minute 30 is quiet");
        assert_eq!(at(true, 0, H_PLUS), 40_000, "minute 40 is in the H+ slot");
        assert_eq!(at(true, 12, H_PLUS), 7_500, "from minute 42 only the tail");
        assert_eq!(at(true, 100, 0), 7_500, "past the curve: the last point");
    }

    /// Catches: an uncovered lease counted at its quiet start: its first two minutes are
    /// charged the larger of minutes 0 and 1, a covered lease is not, and from minute 2
    /// on it is charged the curve.
    #[test]
    fn an_uncovered_lease_is_charged_the_larger_of_its_first_two_minutes() {
        let p = profile(&[(1_000, 0), (30_000, 0), (1_000, 0)], 10_000, GIB);
        let at = |covered, age, slot| p.cpu_charge(10_000, covered, true, age, slot);
        assert_eq!(at(false, 0, 0), 30_000);
        assert_eq!(at(false, 0, 1), 30_000);
        assert_eq!(at(false, 0, 2), 7_500);
        assert_eq!(at(true, 0, 0), 7_500);
        assert_eq!(
            at(false, 1, 0),
            30_000,
            "age 1 is still the first two minutes"
        );
        assert_eq!(at(false, 2, 0), 7_500);
    }

    /// Catches: memory charged the point instead of the remaining maximum (a lease past
    /// its peak still counted at it, or one before its peak not), the `held` floor
    /// ignored, and the tail not held.
    #[test]
    fn memory_is_the_remaining_maximum_never_below_what_is_held() {
        let p = profile(
            &[(0, 2 * GIB), (0, 8 * GIB), (0, 8 * GIB), (0, GIB)],
            1_000,
            8 * GIB,
        );
        let at = |age, slot, held| p.mem_charge(8 * GIB, true, age, slot, held);
        assert_eq!(at(0, 0, 0), 8 * GIB, "the peak is ahead");
        assert_eq!(at(0, 3, 0), GIB, "from minute 3 on");
        assert_eq!(at(0, 4, 0), GIB, "the tail holds the last point");
        assert_eq!(at(3, 0, 0), GIB);
        assert_eq!(at(3, 0, 3 * GIB), 3 * GIB, "held above the curve");
        assert_eq!(at(0, 3, 3 * GIB), 3 * GIB, "and held for every later slot");
        assert_eq!(p.mem_remaining(1), 8 * GIB);
    }

    /// Catches: the flat form of a lease with a life ending late or never (the present
    /// minute is always charged), and a charge that outlives its life in either resource.
    #[test]
    fn a_charge_ends_at_the_expected_life_but_the_present_minute_is_always_charged() {
        let p = Profile::new(
            Choice {
                curve: None,
                cpu: true,
                mem: true,
                demoted_cpu: false,
                demoted_mem: false,
                life: Some(3),
                floor_permille: 750,
                guard_key: 7,
            },
            &cost(2_000, GIB),
        );
        assert!(p.timed());
        assert_eq!(p.new_cpu(0), 2_000);
        assert_eq!(p.new_cpu(2), 2_000);
        assert_eq!(p.new_cpu(3), 0);
        assert_eq!(p.new_cpu(H_PLUS), 0);
        assert_eq!(p.new_mem(2), GIB);
        assert_eq!(p.new_mem(3), 0);
        assert_eq!(
            p.cpu_charge(2_000, true, false, 2, 0),
            2_000,
            "present minute"
        );
        assert_eq!(p.cpu_charge(2_000, true, false, 2, 1), 0);
    }

    /// Catches: the misfit test at the wrong edge: twice the charge exactly is not a
    /// misfit, nor is a jump under 250 m, and a flat-charged resource cannot misfit.
    #[test]
    fn misfits_are_over_twice_the_charge_by_250_m_and_over_the_remaining_maximum() {
        let p = profile(&[(10_000, GIB), (400, 2 * GIB)], 100, 2 * GIB);
        // minute 0: charge max(10000, 75) = 10000; minute 1: floor 75 vs 400 = 400.
        assert!(!p.cpu_misfit(0, 20_000), "exactly twice");
        assert!(p.cpu_misfit(0, 20_001));
        assert!(!p.cpu_misfit(1, 800), "exactly twice");
        assert!(p.cpu_misfit(1, 801), "twice and 250 m over");
        let small = profile(&[(100, GIB)], 100, GIB);
        assert!(
            !small.cpu_misfit(0, 349),
            "under 250 m over the charge of 100"
        );
        assert!(small.cpu_misfit(0, 350));
        assert!(!p.mem_misfit(1, 2 * GIB));
        assert!(p.mem_misfit(1, 2 * GIB + 1));
        assert!(
            !p.mem_misfit(0, 2 * GIB),
            "the remaining maximum from minute 0 is 2 GiB"
        );
        let flat = Profile::new(
            Choice {
                curve: Some(curve(&[(100, GIB)])),
                cpu: false,
                mem: false,
                demoted_cpu: true,
                demoted_mem: true,
                life: None,
                floor_permille: 750,
                guard_key: 7,
            },
            &cost(100, GIB),
        );
        assert!(!flat.cpu_misfit(0, 9_999_999));
        assert!(!flat.mem_misfit(0, u64::MAX));
        assert!(!flat.timed());
    }
}
