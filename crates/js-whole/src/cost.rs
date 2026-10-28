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
}

/// The hosts ordered by drain cost, smallest first: the latest end bound of the work on
/// the host minus `now`, then the fewest leases, then the lowest host id.
pub fn drain_cost(hosts: &[HostLoad], now: u64) -> Vec<DrainCost> {
    let mut out: Vec<DrainCost> = hosts
        .iter()
        .map(|h| DrainCost {
            id: h.id.clone(),
            wait_ms: h
                .end_bounds
                .iter()
                .max()
                .map_or(0, |end| end.saturating_sub(now)),
            leases: h.leases,
        })
        .collect();
    out.sort_by(|a, b| (a.wait_ms, a.leases, &a.id).cmp(&(b.wait_ms, b.leases, &b.id)));
    out
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
        }
    }

    fn ids(v: &[DrainCost]) -> Vec<&str> {
        v.iter().map(|c| c.id.as_str()).collect()
    }

    #[test]
    fn smallest_latest_end_bound_wins() {
        // Kills: ordering by the earliest bound, by the sum, or largest first. Host b
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
        // Kills: skipping the lease-count tiebreak (input order would put a first).
        let hosts = [load("a", 3, &[400]), load("b", 1, &[400])];
        assert_eq!(ids(&drain_cost(&hosts, 0)), ["b", "a"]);
    }

    #[test]
    fn tie_again_takes_lowest_id() {
        // Kills: keeping input order on a full tie.
        let hosts = [
            load("c", 1, &[400]),
            load("a", 1, &[400]),
            load("b", 1, &[400]),
        ];
        assert_eq!(ids(&drain_cost(&hosts, 0)), ["a", "b", "c"]);
    }

    #[test]
    fn idle_host_costs_zero_and_beats_all() {
        // Kills: a host with no work sorting last, or costing now.
        let hosts = [load("a", 1, &[50]), load("z", 0, &[])];
        let got = drain_cost(&hosts, 10);
        assert_eq!(ids(&got), ["z", "a"]);
        assert_eq!(got[0].wait_ms, 0);
        assert_eq!(got[0].leases, 0);
    }

    #[test]
    fn a_bound_already_passed_costs_zero_without_underflow() {
        // Kills: plain subtraction (panics in debug, wraps in release).
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

    fn cfg(deadline: Option<u64>, attempts: u32) -> WholeNodeConfig {
        WholeNodeConfig {
            drain_deadline_ms: deadline,
            max_reserve_attempts: attempts,
            ..WholeNodeConfig::dedicated(2)
        }
    }

    #[test]
    fn deadline_below_the_attempt_cap_is_the_configured_one() {
        // Kills: always returning max wall.
        assert_eq!(reserve_deadline(0, &cfg(Some(60), 2), 1000), 60);
        assert_eq!(reserve_deadline(1, &cfg(Some(60), 2), 1000), 60);
    }

    #[test]
    fn unset_deadline_is_max_wall() {
        // Kills: treating None as zero.
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
        // Kills: max() instead of the max wall at the cap; the bound is max wall.
        assert_eq!(reserve_deadline(2, &cfg(Some(5000), 2), 1000), 1000);
    }
}
