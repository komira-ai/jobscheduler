//! The head-of-queue headroom reservation: which worker a blocked request holds.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use js_types::sched::LeaseId;

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

/// The first instant (farm milliseconds, no earlier than `now`) at which a lease that
/// needs `need` (cores, memory) fits on a worker, against the caps `limits`: the left
/// side `left` with the charge of each lease in `leases` (lease, cores, memory) taken off
/// at that lease's end in `ends`. The measured load and working set of `left` stay as they
/// are. A lease whose end is in the past is released at `now`; a lease with no end in
/// `ends` is never released. `None` when the lease does not fit even after every lease
/// with an end is released.
///
/// The fit is the flat predicate: the CPU and memory caps against the left side. It does
/// not model the horizon curves, the flat-memory guard or the cold cap. That is safe in
/// the direction that matters: those limits can only make the real start later, so the
/// start time here is a lower bound of the start the full predicate allows, and a
/// backfill gate that admits work only if it ends before this start (`fits_in_tail`:
/// end no later than the start) admits only work that also ends before the real start.
/// It costs some backfill and never starves the head.
///
/// Among heads whose start time is unknown or equal, the caller ranks by the binding
/// ratio, which keeps the order the hold had before start times.
pub(crate) fn start_time(
    left: &Left,
    leases: &[(LeaseId, u64, u64)],
    ends: &BTreeMap<LeaseId, Option<u64>>,
    limits: (u64, u64),
    need: (u64, u64),
    now: u64,
) -> Option<u64> {
    let (cpu_cap, mem_cap) = limits;
    let (need_cpu, need_mem) = need;
    let mut releases: Vec<(u64, u64, u64)> = leases
        .iter()
        .filter_map(|(lease, cpu, mem)| {
            let end = ends.get(lease).copied().flatten()?;
            Some((end.max(now), *cpu, *mem))
        })
        .collect();
    releases.sort_unstable();
    let (mut freed_cpu, mut freed_mem) = (0u64, 0u64);
    let mut next = 0;
    let mut at = now;
    loop {
        while let Some(&(_, cpu, mem)) = releases.get(next).filter(|r| r.0 <= at) {
            freed_cpu = freed_cpu.saturating_add(cpu);
            freed_mem = freed_mem.saturating_add(mem);
            next += 1;
        }
        // What stays when the leases are gone is the measured load and working set.
        let cpu = left.cpu.saturating_sub(freed_cpu).max(left.measured);
        let mem = left.mem.saturating_sub(freed_mem).max(left.working_set);
        if cpu.saturating_add(need_cpu) <= cpu_cap && mem.saturating_add(need_mem) <= mem_cap {
            return Some(at);
        }
        at = releases.get(next)?.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admission::{LoadConfig, WorkerLoad};
    use js_types::sched::usage::LeaseCost;
    use js_types::sched::{FarmTime, Resources};

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

    fn lease_cost(cpu: u64, cold: bool) -> LeaseCost {
        LeaseCost {
            cpu_millis: cpu,
            memory_bytes: 0,
            limit_bytes: 0,
            cold,
            retry: false,
            key_hash: 0,
        }
    }

    /// A worker of 8 cores holding four leases of a core, one cold lease already
    /// admitted in this window, and the first of `releases` leases taken off.
    fn worker(releases: u64) -> WorkerLoad {
        let at = FarmTime::from_millis(0);
        let mut load = WorkerLoad::default();
        for n in 1..=4 {
            load.insert(lease(n), lease_cost(1_000, false), false, None);
        }
        load.note_cold(at, &LoadConfig::default());
        for n in 1..=releases {
            load.remove(lease(n));
        }
        load
    }

    /// The first instant of `steps` (instant, leases released by then) at which the full
    /// predicate admits a head of 6 cores, `cold` or not; `None` when it never does.
    fn full_start(cold: bool, steps: &[(u64, u64)]) -> Option<u64> {
        let cfg = LoadConfig::default();
        let capacity = Resources::new(8_000, 32 * GIB);
        let at = FarmTime::from_millis(0);
        steps.iter().find_map(|&(instant, released)| {
            let load = worker(released);
            let left = load.left(at, &capacity, &cfg);
            let cost = lease_cost(6_000, cold);
            load.admits(&cost, None, &left, &capacity, at, &cfg)
                .is_ok()
                .then_some(instant)
        })
    }

    /// Catches: a start time later than the one the full predicate allows. The leases end
    /// two at 100 and two at 900; a cold head is also held by the cold cap, which refuses
    /// it however many leases end (the cap is full in this window), so its real start is
    /// later (here, never), and the projected start (100) is no later than that: a gate
    /// that admits only work ending by 100 never delays it. A head that is not cold has no
    /// other limit, and its projected start is the real one. The backfill gate
    /// (`fits_in_tail`) is not called on this path yet, so the property is tested at the
    /// level that exists: the projection against the full predicate.
    #[test]
    fn the_projected_start_is_no_later_than_the_start_the_full_predicate_allows() {
        let cfg = LoadConfig::default();
        let capacity = Resources::new(8_000, 32 * GIB);
        let load = worker(0);
        let left = load.left(FarmTime::from_millis(0), &capacity, &cfg);
        let ends = ends([Some(100), Some(100), Some(900), Some(900)]);
        let limits = (cfg.cpu_limit(&capacity), cfg.mem_limit(&capacity));
        let projected = start_time(&left, &load.charges(&left), &ends, limits, (6_000, 0), 0);
        let steps = [(0, 0), (100, 2), (900, 4)];
        let plain = full_start(false, &steps);
        let cold = full_start(true, &steps);
        assert_eq!(projected, Some(100));
        assert_eq!(plain, Some(100), "no other limit: the projection is exact");
        assert_eq!(cold, None, "the cold cap refuses it in every step");
        for real in [plain, cold] {
            assert!(real.is_none_or(|real| projected.is_some_and(|p| p <= real)));
        }
    }
}
