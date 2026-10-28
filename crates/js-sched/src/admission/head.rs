//! The head-of-queue headroom reservation: which worker a blocked request holds.

use std::cmp::Ordering;

use js_types::sched::WorkerId;

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
}
