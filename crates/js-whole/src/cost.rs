//! The drain cost order (design section 3) and the reservation deadline (section 4).

use crate::config::WholeNodeConfig;

/// What the scheduler's table shows on one host, as facts for [`drain_cost`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostLoad {
    /// The host id.
    pub id: String,
    /// Leases on the host.
    pub leases: usize,
    /// The end bound of each lease and each placed run: its start plus its max wall
    /// time, as an absolute time on the scheduler's clock.
    pub end_bounds: Vec<u64>,
    /// The estimated end of each lease and each placed run, in the same order as
    /// `end_bounds` and on the same clock; `None`, or an entry past the end of this list,
    /// is a lease with no estimate. An empty list is the case of no estimates at all.
    pub est_ends: Vec<Option<u64>>,
}

/// A host with its drain cost, in the order a reservation should try it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrainCost {
    /// The host id.
    pub id: String,
    /// The latest end bound minus now, saturating at zero; zero for a host with none.
    pub wait_ms: u64,
    /// Leases on the host.
    pub leases: usize,
    /// The host's tail as an absolute time ([`tail`]); `None` when a lease on it has no
    /// estimate and no max-wall bound.
    pub tail_end: Option<u64>,
}

/// The hosts ordered by drain cost, smallest first: the host's tail (see [`tail`]) minus
/// `now`, then the fewest leases, then the lowest host id. A host with an unknown tail
/// (a lease with neither an estimate nor a max-wall bound) ranks after every host with
/// a known one; among those the order is the fewest leases, then the lowest host id.
/// With no estimates given, a lease's end is its max-wall bound, so the order is that of
/// the latest end bound.
pub fn drain_cost(hosts: &[HostLoad], now: u64) -> Vec<DrainCost> {
    let mut out: Vec<DrainCost> = hosts
        .iter()
        .map(|h| {
            let tail_end = tail(h);
            DrainCost {
                id: h.id.clone(),
                wait_ms: tail_end.unwrap_or(u64::MAX).saturating_sub(now),
                leases: h.leases,
                tail_end,
            }
        })
        .collect();
    out.sort_by(|a, b| {
        let key = |c: &DrainCost| (c.tail_end.is_none(), c.wait_ms, c.leases);
        (key(a), &a.id).cmp(&(key(b), &b.id))
    });
    out
}

/// The default margin [`fits_in_tail`] keeps between backfilled work and the end of the
/// tail: one minute. Nothing in production uses it yet; the backfill gate (step 3) will.
pub const DEFAULT_BACKFILL_MARGIN_MS: u64 = 60_000;

/// The end of one lease: its estimate cut to its max-wall bound when it has one, else the
/// bound, else unknown. `u64::MAX` is the bound of a lease with no max wall.
fn lease_end(bound: u64, estimate: Option<u64>) -> Option<u64> {
    match (estimate, bound == u64::MAX) {
        (Some(est), false) => Some(est.min(bound)),
        (Some(est), true) => Some(est),
        (None, false) => Some(bound),
        (None, true) => None,
    }
}

/// The tail of `load`: the latest end of its leases as an absolute time, each end being
/// the lease's estimate cut to its max-wall bound ([`HostLoad::est_ends`]), or the bound
/// when the lease has no estimate. A host with no lease has tail zero. `None` when some
/// lease has neither an estimate nor a bound.
///
/// A caller that reserves a host takes the tail once, when it sets the mark, and keeps
/// that value: work admitted later cannot extend it.
pub fn tail(load: &HostLoad) -> Option<u64> {
    let mut ends = load.end_bounds.iter().enumerate().map(|(i, bound)| {
        let estimate = load.est_ends.get(i).copied().flatten();
        lease_end(*bound, estimate)
    });
    ends.try_fold(0, |latest: u64, end| end.map(|end| latest.max(end)))
}

/// Whether work estimated to take `estimated_duration` may start at `now` on a reserved
/// host whose tail ends at `tail_end`: it must end, with `margin` to spare, no later
/// than the tail. Work with no estimate never fits, and nothing fits a host whose tail is
/// unknown: the check fails closed.
///
/// Nothing in production calls this yet; the backfill gate (step 3) will.
pub fn fits_in_tail(
    now: u64,
    estimated_duration: Option<u64>,
    tail_end: Option<u64>,
    margin: u64,
) -> bool {
    estimated_duration
        .zip(tail_end)
        .is_some_and(|(duration, tail_end)| {
            now.saturating_add(duration).saturating_add(margin) <= tail_end
        })
}

/// How long a reservation may drain before it is given up. After `attempts_given_up`
/// reaches `max_reserve_attempts` the deadline is `max_wall_ms` whatever
/// `drain_deadline_ms` says; before that it is `drain_deadline_ms`, or `max_wall_ms`
/// when none is set.
pub fn reserve_deadline(attempts_given_up: u32, cfg: &WholeNodeConfig, max_wall_ms: u64) -> u64 {
    if attempts_given_up >= cfg.max_reserve_attempts {
        max_wall_ms
    } else {
        cfg.drain_deadline_ms(max_wall_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(id: &str, leases: usize, ends: &[u64]) -> HostLoad {
        HostLoad {
            id: id.to_string(),
            leases,
            end_bounds: ends.to_vec(),
            est_ends: Vec::new(),
        }
    }

    fn ids(v: &[DrainCost]) -> Vec<&str> {
        v.iter().map(|c| c.id.as_str()).collect()
    }

    #[test]
    fn smallest_latest_end_bound_wins() {
        // Catches: ordering by the earliest bound, by the sum, or largest first. Host b
        // has the earliest single bound but the latest one is what counts.
        let hosts = [
            load("a", 1, &[500]),
            load("b", 2, &[100, 900]),
            load("c", 1, &[300]),
        ];
        let got = drain_cost(&hosts, 100);
        assert_eq!(ids(&got), ["c", "a", "b"]);
        assert_eq!(got[0].wait_ms, 200);
        assert_eq!(got[2].wait_ms, 800);
    }

    #[test]
    fn tie_on_bound_takes_fewest_leases() {
        // Catches: skipping the lease-count tiebreak (input order would put a first).
        let hosts = [load("a", 3, &[400]), load("b", 1, &[400])];
        assert_eq!(ids(&drain_cost(&hosts, 0)), ["b", "a"]);
    }

    #[test]
    fn tie_again_takes_lowest_id() {
        // Catches: keeping input order on a full tie.
        let hosts = [
            load("c", 1, &[400]),
            load("a", 1, &[400]),
            load("b", 1, &[400]),
        ];
        assert_eq!(ids(&drain_cost(&hosts, 0)), ["a", "b", "c"]);
    }

    #[test]
    fn idle_host_costs_zero_and_beats_all() {
        // Catches: a host with no work sorting last, or costing now.
        let hosts = [load("a", 1, &[50]), load("z", 0, &[])];
        let got = drain_cost(&hosts, 10);
        assert_eq!(ids(&got), ["z", "a"]);
        assert_eq!(got[0].wait_ms, 0);
        assert_eq!(got[0].leases, 0);
    }

    #[test]
    fn a_bound_already_passed_costs_zero_without_underflow() {
        // Catches: plain subtraction (panics in debug, wraps in release).
        let hosts = [load("a", 1, &[5])];
        assert_eq!(drain_cost(&hosts, 100)[0].wait_ms, 0);
    }

    #[test]
    fn full_ties_across_many_hosts_order_by_lease_count_then_id() {
        // Five hosts share one bound; the order is fewest leases, then lowest id, whatever
        // order they arrive in.
        let hosts = [
            load("e", 2, &[400]),
            load("c", 1, &[400]),
            load("d", 2, &[400]),
            load("b", 1, &[400]),
            load("a", 3, &[400]),
        ];
        assert_eq!(ids(&drain_cost(&hosts, 0)), ["b", "c", "d", "e", "a"]);
        let mut reversed = hosts.clone();
        reversed.reverse();
        assert_eq!(ids(&drain_cost(&reversed, 0)), ["b", "c", "d", "e", "a"]);
    }

    #[test]
    fn no_hosts_no_candidates() {
        assert!(drain_cost(&[], 0).is_empty());
    }

    /// The `drain_cost` of the code before the tail rule, verbatim, kept as an oracle.
    /// It returns `(id, wait_ms, leases)` since the old `DrainCost` had no `tail_end`.
    fn drain_cost_before_tail(hosts: &[HostLoad], now: u64) -> Vec<(String, u64, usize)> {
        let mut out: Vec<(String, u64, usize)> = hosts
            .iter()
            .map(|h| {
                (
                    h.id.clone(),
                    h.end_bounds
                        .iter()
                        .max()
                        .map_or(0, |end| end.saturating_sub(now)),
                    h.leases,
                )
            })
            .collect();
        out.sort_by(|a, b| (a.1, a.2, &a.0).cmp(&(b.1, b.2, &b.0)));
        out
    }

    fn fields(v: &[DrainCost]) -> Vec<(String, u64, usize)> {
        v.iter()
            .map(|c| (c.id.clone(), c.wait_ms, c.leases))
            .collect()
    }

    #[test]
    fn with_no_estimates_every_old_case_gives_the_old_order_and_waits() {
        // Catches: any change in id, wait_ms, leases or order for the inputs the
        // pre-tail tests used, at several clocks; tail_end is checked on its own.
        let cases: Vec<Vec<HostLoad>> = vec![
            vec![
                load("a", 1, &[500]),
                load("b", 2, &[100, 900]),
                load("c", 1, &[300]),
            ],
            vec![load("a", 3, &[400]), load("b", 1, &[400])],
            vec![
                load("c", 1, &[400]),
                load("a", 1, &[400]),
                load("b", 1, &[400]),
            ],
            vec![load("a", 1, &[50]), load("z", 0, &[])],
            vec![load("a", 1, &[5])],
            vec![
                load("e", 2, &[400]),
                load("c", 1, &[400]),
                load("d", 2, &[400]),
                load("b", 1, &[400]),
                load("a", 3, &[400]),
            ],
            vec![],
            vec![
                load("c", 1, &[NO_BOUND]),
                load("b", 2, &[NO_BOUND, NO_BOUND]),
                load("a", 1, &[NO_BOUND]),
                load("k", 1, &[1_000]),
            ],
        ];
        for hosts in cases {
            for now in [0, 10, 100, 450, 1_000_000] {
                let new = drain_cost(&hosts, now);
                assert_eq!(fields(&new), drain_cost_before_tail(&hosts, now));
                for cost in &new {
                    let h = hosts.iter().find(|h| h.id == cost.id).unwrap();
                    let want = match h.end_bounds.iter().max().copied() {
                        None => Some(0),
                        Some(NO_BOUND) => None,
                        Some(m) => Some(m),
                    };
                    assert_eq!(cost.tail_end, want, "host {}", cost.id);
                }
            }
        }
    }

    #[test]
    fn a_host_with_no_max_wall_ranks_after_one_with_a_finite_bound() {
        // Catches: a bound-less lease ranking first or by its known part, and its wait
        // not being the old `u64::MAX - now`.
        let hosts = [load("a", 1, &[NO_BOUND]), load("b", 1, &[7_000])];
        let now = 1_000;
        let got = drain_cost(&hosts, now);
        assert_eq!(ids(&got), ["b", "a"]);
        assert_eq!(got[0].wait_ms, 6_000);
        assert_eq!(got[1].wait_ms, u64::MAX - now);
        assert_eq!(got[1].tail_end, None);
        assert_eq!(fields(&got), drain_cost_before_tail(&hosts, now));
    }

    fn with_est(id: &str, bounds: &[u64], ests: &[Option<u64>]) -> HostLoad {
        HostLoad {
            id: id.to_string(),
            leases: bounds.len(),
            end_bounds: bounds.to_vec(),
            est_ends: ests.to_vec(),
        }
    }

    const NO_BOUND: u64 = u64::MAX;

    #[test]
    fn the_shorter_estimated_tail_wins_over_the_shorter_max_wall() {
        // Catches: ordering by the max-wall bound. Host z has the longer bound (10_000) but
        // is estimated to end at 2_000; host a has bound 5_000 and no estimate. The ids
        // are chosen so the id tie-break would pick the other host.
        let hosts = [
            with_est("a-short-wall", &[5_000], &[None]),
            with_est("z-long-wall-short-tail", &[10_000], &[Some(2_000)]),
        ];
        let got = drain_cost(&hosts, 0);
        assert_eq!(ids(&got), ["z-long-wall-short-tail", "a-short-wall"]);
        assert_eq!(got[0].wait_ms, 2_000);
        assert_eq!(got[0].tail_end, Some(2_000));
        assert_eq!(got[1].tail_end, Some(5_000));
    }

    #[test]
    fn an_estimate_past_the_max_wall_is_cut_to_it() {
        // Catches: trusting an estimate beyond the hard bound.
        let hosts = [
            with_est("a", &[1_000], &[Some(9_000)]),
            with_est("b", &[3_000], &[Some(2_000)]),
        ];
        let got = drain_cost(&hosts, 0);
        assert_eq!(ids(&got), ["a", "b"]);
        assert_eq!(got[0].tail_end, Some(1_000));
    }

    #[test]
    fn a_host_with_an_unknown_lease_ranks_after_every_known_tail() {
        // Catches: ranking an unknown tail by its known part, or first. Host u has one
        // lease that ends at 100 and one with neither an estimate nor a bound; host k
        // has a far tail. Host e ends at 7_000 by estimate alone (no bound).
        let hosts = [
            with_est("u", &[100, NO_BOUND], &[Some(100), None]),
            with_est("k", &[1_000_000], &[None]),
            with_est("e", &[NO_BOUND], &[Some(7_000)]),
        ];
        let got = drain_cost(&hosts, 0);
        assert_eq!(ids(&got), ["e", "k", "u"]);
        assert_eq!(got[2].tail_end, None);
        assert_eq!(got[2].wait_ms, u64::MAX);
    }

    #[test]
    fn unknown_hosts_keep_the_lease_count_then_id_order() {
        // Catches: dropping the existing tie-breaks inside the unknown group.
        let hosts = [
            load("c", 1, &[NO_BOUND]),
            load("b", 2, &[NO_BOUND, NO_BOUND]),
            load("a", 1, &[NO_BOUND]),
        ];
        assert_eq!(ids(&drain_cost(&hosts, 5)), ["a", "c", "b"]);
    }

    #[test]
    fn no_estimates_gives_the_order_by_latest_bound() {
        // The old behaviour is the no-estimate case: an empty list and a list of None
        // order the same, and a bound-less lease is last with the old wait.
        let plain = [
            load("a", 1, &[500]),
            load("b", 2, &[100, 900]),
            load("n", 1, &[NO_BOUND]),
            load("c", 1, &[300]),
        ];
        let nones: Vec<HostLoad> = plain
            .iter()
            .map(|h| HostLoad {
                est_ends: vec![None; h.end_bounds.len()],
                ..h.clone()
            })
            .collect();
        let got = drain_cost(&plain, 100);
        assert_eq!(ids(&got), ["c", "a", "b", "n"]);
        assert_eq!(got[3].wait_ms, u64::MAX - 100);
        let waits = |v: &[DrainCost]| v.iter().map(|c| c.wait_ms).collect::<Vec<_>>();
        assert_eq!(ids(&drain_cost(&nones, 100)), ids(&got));
        assert_eq!(waits(&drain_cost(&nones, 100)), waits(&got));
    }

    #[test]
    fn tail_clamps_each_estimate_and_falls_back_to_the_bound() {
        // Catches: max over the raw estimates; no clamp; no fallback; wrong index pairing.
        assert_eq!(tail(&with_est("h", &[100], &[Some(500)])), Some(100));
        assert_eq!(tail(&with_est("h", &[100], &[Some(50)])), Some(50));
        assert_eq!(tail(&with_est("h", &[NO_BOUND], &[Some(50)])), Some(50));
        assert_eq!(tail(&with_est("h", &[100], &[None])), Some(100));
        assert_eq!(tail(&with_est("h", &[NO_BOUND], &[None])), None);
        // The latest of the clamped ends, not of the bounds.
        let two = with_est("h", &[900, 400], &[Some(200), Some(700)]);
        assert_eq!(tail(&two), Some(400));
        // An estimate list shorter than the bounds leaves the rest without estimates.
        let short = with_est("h", &[900, 400], &[Some(200)]);
        assert_eq!(tail(&short), Some(400));
        // One unknown lease makes the whole tail unknown.
        assert_eq!(tail(&with_est("h", &[100, NO_BOUND], &[None, None])), None);
    }

    #[test]
    fn once_the_running_leases_are_gone_the_tail_is_over() {
        // The whole-node job starts when the tail is over: an empty host has tail zero
        // and ranks first with no wait, and estimates already passed cost no wait.
        let hosts = [load("busy", 1, &[9_000]), load("gone", 0, &[])];
        let got = drain_cost(&hosts, 4_000);
        assert_eq!(ids(&got), ["gone", "busy"]);
        assert_eq!(got[0].wait_ms, 0);
        assert_eq!(got[0].tail_end, Some(0));
        let over = with_est("over", &[9_000], &[Some(50)]);
        assert_eq!(drain_cost(&[over], 100)[0].wait_ms, 0);
    }

    #[test]
    fn work_fits_up_to_the_tail_end_less_the_margin_and_not_a_millisecond_more() {
        // Catches: < for <=, forgetting the margin, adding it twice.
        let (now, tail_end, margin) = (1_000, 2_000, 100);
        assert!(fits_in_tail(now, Some(900), Some(tail_end), margin));
        assert!(!fits_in_tail(now, Some(901), Some(tail_end), margin));
        assert!(fits_in_tail(now, Some(0), Some(1_100), 100));
        assert!(!fits_in_tail(now, Some(0), Some(1_099), 100));
    }

    #[test]
    fn work_with_an_unknown_estimate_or_a_host_with_an_unknown_tail_never_fits() {
        // Catches: failing open.
        assert!(!fits_in_tail(0, None, Some(u64::MAX), 0));
        assert!(!fits_in_tail(0, Some(1), None, 0));
        assert!(!fits_in_tail(0, None, None, 0));
    }

    #[test]
    fn the_fit_sum_saturates_instead_of_wrapping() {
        // Catches: plain addition (wraps in release and would let huge work fit).
        assert!(!fits_in_tail(10, Some(u64::MAX), Some(100), 5));
        assert!(!fits_in_tail(u64::MAX, Some(1), Some(100), 0));
    }

    fn cfg(deadline: Option<u64>, attempts: u32) -> WholeNodeConfig {
        WholeNodeConfig {
            drain_deadline_ms: deadline,
            max_reserve_attempts: attempts,
            ..WholeNodeConfig::dedicated(2)
        }
    }

    #[test]
    fn deadline_below_the_attempt_cap_is_the_configured_one() {
        // Catches: always returning max wall.
        assert_eq!(reserve_deadline(0, &cfg(Some(60), 2), 1000), 60);
        assert_eq!(reserve_deadline(1, &cfg(Some(60), 2), 1000), 60);
    }

    #[test]
    fn unset_deadline_is_max_wall() {
        // Catches: treating None as zero.
        assert_eq!(reserve_deadline(0, &cfg(None, 2), 1000), 1000);
    }

    #[test]
    fn at_the_attempt_cap_the_deadline_is_max_wall_even_if_shorter_is_set() {
        // W6: kills `>` for `>=`, and honouring a shorter drain_deadline forever.
        assert_eq!(reserve_deadline(2, &cfg(Some(60), 2), 1000), 1000);
        assert_eq!(reserve_deadline(7, &cfg(Some(60), 2), 1000), 1000);
    }

    #[test]
    fn a_longer_configured_deadline_is_also_cut_to_max_wall_at_the_cap() {
        // Catches: max() instead of the max wall at the cap; the bound is max wall.
        assert_eq!(reserve_deadline(2, &cfg(Some(5000), 2), 1000), 1000);
    }
}
