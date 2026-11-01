//! The head-of-queue headroom reservation: which worker a blocked request holds.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use js_types::sched::{LeaseId, WorkerId};

use super::Left;

/// `num / den`, compared exactly.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Ratio {
    num: u128,
    den: u128,
}

impl Ratio {
    /// `num / den`; a resource the worker has none of (`den` 0) is one it cannot be
    /// short of, so its ratio is 0.
    pub(crate) fn new(num: u64, den: u64) -> Self {
        if den == 0 {
            Self { num: 0, den: 1 }
        } else {
            Self {
                num: u128::from(num),
                den: u128::from(den),
            }
        }
    }

    /// Whether `self` is no more than `best` plus a tenth.
    fn within_tenth_of(self, best: Self) -> bool {
        self.num.saturating_mul(best.den).saturating_mul(10)
            <= best
                .num
                .saturating_mul(10)
                .saturating_add(best.den)
                .saturating_mul(self.den)
    }
}

impl Ord for Ratio {
    fn cmp(&self, other: &Self) -> Ordering {
        self.num
            .saturating_mul(other.den)
            .cmp(&other.num.saturating_mul(self.den))
    }
}

impl PartialOrd for Ratio {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Ratio {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Ratio {}

/// The binding ratio of a worker for a request: `max((L_cpu + p) / C_cpu, (L_mem + m) /
/// C_mem)`, the larger of the shares of CPU and of memory the worker would be at with the
/// request on it.
pub(crate) fn binding(cpu: (u64, u64), mem: (u64, u64)) -> Ratio {
    Ratio::new(cpu.0, cpu.1).max(Ratio::new(mem.0, mem.1))
}

/// The worker a blocked head holds, of `candidates` (in name order, each with its binding
/// ratio): the one with the smallest ratio, the first by name among equals, except that
/// the worker `kept` from the last round stays while its ratio is within a tenth of the
/// best, so the hold does not wander between workers whose loads move a little each
/// window.
pub(crate) fn pick<'a>(
    candidates: &[(&'a WorkerId, Ratio)],
    kept: Option<&WorkerId>,
) -> Option<&'a WorkerId> {
    let &(best_name, best) = candidates.iter().min_by_key(|(_, ratio)| *ratio)?;
    let sticky = candidates
        .iter()
        .find(|(name, _)| Some(*name) == kept)
        .filter(|(_, ratio)| ratio.within_tenth_of(best));
    Some(sticky.map_or(best_name, |(name, _)| *name))
}

/// The first instant (farm milliseconds, no earlier than `now`) at which a lease that
/// needs `need` (cores, memory) fits on a worker, against the caps `limits`: the left
/// side `left` with the charge of each lease in `leases` (lease, cores, memory) taken off
/// at that lease's end in `ends`. The measured load and working set of `left` stay as they
/// are. A lease whose end is in the past is released at `now`; a lease with no end in
/// `ends` is never released. `None` when the lease does not fit even after every lease
/// with an end is released.
pub(crate) fn start_time(
    left: &Left,
    leases: &[(LeaseId, u64, u64)],
    ends: &BTreeMap<LeaseId, Option<u64>>,
    limits: (u64, u64),
    need: (u64, u64),
    now: u64,
) -> Option<u64> {
    let _ = (left, leases, ends, limits, need, now);
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(name: &str) -> WorkerId {
        WorkerId::new(name)
    }

    fn r(num: u64, den: u64) -> Ratio {
        Ratio::new(num, den)
    }

    /// Catches: a binding ratio that takes the CPU share alone or the memory share
    /// alone, or a worker without a resource dividing by zero.
    #[test]
    fn the_binding_ratio_is_the_larger_share() {
        assert_eq!(binding((50, 100), (10, 100)), r(1, 2));
        assert_eq!(binding((10, 100), (75, 100)), r(3, 4));
        assert_eq!(binding((9, 0), (1, 4)), r(1, 4));
        assert_eq!(binding((9, 0), (9, 0)), r(0, 1));
    }

    /// Catches: ratios compared by float division, or by a product that overflows.
    #[test]
    fn ratios_compare_exactly() {
        assert!(r(u64::MAX - 1, u64::MAX) < r(u64::MAX, u64::MAX));
        assert_eq!(r(1, 3), r(2_000, 6_000));
        assert!(r(1, 3) < r(34, 100));
    }

    /// Catches: a tie going to the later worker, or the smallest ratio not winning.
    #[test]
    fn the_smallest_ratio_wins_and_a_tie_goes_to_the_first_name() {
        let (a, b, c) = (w("a"), w("b"), w("c"));
        let all = [(&a, r(8, 10)), (&b, r(5, 10)), (&c, r(5, 10))];
        assert_eq!(pick(&all, None), Some(&b));
        assert_eq!(pick(&[], None), None);
    }

    /// Catches: a hold that moves for every wobble (the kept worker leaves at once), a
    /// stickiness without a limit (the kept worker stays however loaded), and an edge at
    /// exactly a tenth that goes the wrong way.
    #[test]
    fn the_kept_worker_stays_within_a_tenth_of_the_best() {
        let (a, b) = (w("a"), w("b"));
        let near = [(&a, r(50, 100)), (&b, r(60, 100))];
        assert_eq!(pick(&near, Some(&b)), Some(&b), "exactly a tenth over");
        let far = [(&a, r(50, 100)), (&b, r(61, 100))];
        assert_eq!(pick(&far, Some(&b)), Some(&a), "just past a tenth");
        assert_eq!(pick(&far, Some(&a)), Some(&a));
        assert_eq!(
            pick(&near, Some(&w("gone"))),
            Some(&a),
            "kept is not a candidate"
        );
    }

    const GIB: u64 = 1 << 30;

    fn left(cpu: u64, mem: u64, measured: u64, working_set: u64) -> Left {
        Left {
            cpu,
            mem,
            fallback: measured == 0,
            floor_cpu: 0,
            measured,
            working_set,
        }
    }

    fn lease(n: u64) -> LeaseId {
        LeaseId::new(1, n)
    }

    /// Four leases of a core each.
    fn four() -> Vec<(LeaseId, u64, u64)> {
        (1..=4).map(|n| (lease(n), 1_000, 0)).collect()
    }

    fn ends(known: [Option<u64>; 4]) -> BTreeMap<LeaseId, Option<u64>> {
        (1..=4).zip(known).map(|(n, e)| (lease(n), e)).collect()
    }

    const CAPS: (u64, u64) = (8_800, 32 * GIB);

    /// Catches: a start time later than now for a lease that fits now.
    #[test]
    fn a_lease_that_fits_now_starts_now() {
        let l = left(1_000, 0, 0, 0);
        let got = start_time(&l, &four(), &ends([None; 4]), CAPS, (6_000, 0), 50);
        assert_eq!(got, Some(50));
    }

    /// Catches: the start taken when the worker empties instead of when enough is free: 6
    /// cores fit once two of four leases have ended (at 900), not at the first end (100)
    /// and not at the last.
    #[test]
    fn the_start_is_when_enough_is_released_not_when_the_worker_empties() {
        let l = left(4_000, 0, 0, 0);
        let early = ends([Some(100), Some(100), Some(900), Some(900)]);
        assert_eq!(
            start_time(&l, &four(), &early, CAPS, (6_000, 0), 0),
            Some(100)
        );
        let spread = ends([Some(100), Some(900), Some(900), Some(2_000)]);
        assert_eq!(
            start_time(&l, &four(), &spread, CAPS, (6_000, 0), 0),
            Some(900)
        );
    }

    /// Catches: a lease with no end treated as released at once or at the end of time:
    /// the fit that depends on it has no start time, and one that does not depend on it
    /// has.
    #[test]
    fn a_fit_that_depends_on_an_unknown_end_has_no_start_time() {
        let l = left(4_000, 0, 0, 0);
        let one_known = ends([Some(100), None, None, None]);
        assert_eq!(
            start_time(&l, &four(), &one_known, CAPS, (6_000, 0), 0),
            None
        );
        let two_known = ends([Some(100), Some(200), None, None]);
        assert_eq!(
            start_time(&l, &four(), &two_known, CAPS, (6_000, 0), 0),
            Some(200)
        );
        let none: BTreeMap<LeaseId, Option<u64>> = BTreeMap::new();
        assert_eq!(start_time(&l, &four(), &none, CAPS, (6_000, 0), 0), None);
    }

    /// Catches: an overdue lease (its end already past) kept charged, or released in the
    /// past: both overdue leases are released at now, so the lease starts at now.
    #[test]
    fn an_overdue_lease_is_released_at_now() {
        let l = left(4_000, 0, 0, 0);
        let overdue = ends([Some(100), Some(200), Some(900), Some(900)]);
        assert_eq!(
            start_time(&l, &four(), &overdue, CAPS, (6_000, 0), 500),
            Some(500)
        );
    }

    /// Catches: the measured load released with the leases: it binds at 7 cores however
    /// many leases end, so the lease never fits and has no start time.
    #[test]
    fn the_measured_load_stays_while_leases_end() {
        let l = left(7_000, 0, 7_000, 0);
        let all = ends([Some(100), Some(100), Some(100), Some(100)]);
        assert_eq!(start_time(&l, &four(), &all, CAPS, (6_000, 0), 0), None);
    }

    /// Catches: memory left out of the fit: the leases release 8 GiB each and 16 GiB
    /// are needed on a worker charged 28 of 32, so two must end.
    #[test]
    fn memory_binds_as_well_as_cores() {
        let l = left(0, 28 * GIB, 0, 0);
        let leases: Vec<_> = (1..=3).map(|n| (lease(n), 0, 8 * GIB)).collect();
        let known: BTreeMap<_, _> = (1..=3)
            .zip([100, 200, 300])
            .map(|(n, e)| (lease(n), Some(e)))
            .collect();
        assert_eq!(
            start_time(&l, &leases, &known, CAPS, (0, 16 * GIB), 0),
            Some(200)
        );
    }
}
