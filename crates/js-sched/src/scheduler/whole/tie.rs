//! The tie rule of a whole-machine mark picked again every round by the least tail
//! ([`crate::Scheduler`]'s `hold_whole`): on the same tail the mark stays where it is.

use js_types::sched::WorkerId;
use js_whole::cost::DrainCost;

/// `drain` with the worker `kept` (a whole-machine mark's) moved ahead of every worker
/// whose tail is the same as its own, so that a mark picked again by the least tail stays
/// where it is on a tie instead of moving to a worker with fewer leases, an earlier
/// provider or a lower name. A worker with a shorter tail stays ahead of it. `drain` is in
/// drain cost order (`Scheduler::drain_order`); a `kept` it does not list (a worker gone)
/// changes nothing.
pub(super) fn kept_first(mut drain: Vec<DrainCost>, kept: Option<&WorkerId>) -> Vec<DrainCost> {
    let tail = |cost: &DrainCost| (cost.tail_end.is_none(), cost.wait_ms);
    let at = kept.and_then(|kept| drain.iter().position(|cost| cost.id == kept.as_str()));
    if let Some(at) = at {
        let own = tail(&drain[at]);
        let first = drain[..at]
            .iter()
            .take_while(|cost| tail(cost) != own)
            .count();
        let cost = drain.remove(at);
        drain.insert(first, cost);
    }
    drain
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cost(id: &str, tail_end: Option<u64>, leases: usize) -> DrainCost {
        DrainCost {
            id: id.to_owned(),
            wait_ms: tail_end.unwrap_or(u64::MAX),
            leases,
            tail_end,
            provider_rank: 0,
        }
    }

    fn ids(drain: &[DrainCost]) -> Vec<&str> {
        drain.iter().map(|cost| cost.id.as_str()).collect()
    }

    /// Catches: the kept worker moved ahead of one whose tail is shorter, left behind one
    /// whose tail is the same (by fewer leases or a lower name), or moved ahead of the
    /// rest of its own tie only partly. `a` (10, one lease) is shorter than `c` and `b`
    /// (20, which sort `b` before `c` by leases), and `d` is unknown: kept `c` goes to the
    /// head of its tie, behind `a`.
    #[test]
    fn the_kept_worker_leads_its_tie_and_stays_behind_a_shorter_tail() {
        let drain = vec![
            cost("a", Some(10), 1),
            cost("b", Some(20), 1),
            cost("c", Some(20), 2),
            cost("d", None, 0),
        ];
        let kept = WorkerId::new("c");
        assert_eq!(
            ids(&kept_first(drain.clone(), Some(&kept))),
            ["a", "c", "b", "d"]
        );
        let kept = WorkerId::new("a");
        assert_eq!(ids(&kept_first(drain, Some(&kept))), ["a", "b", "c", "d"]);
    }

    /// Catches: unknown tails not read as a tie with each other, or an order changed with
    /// no kept worker or with one the order does not list (a worker gone).
    #[test]
    fn unknown_tails_tie_and_no_kept_worker_changes_nothing() {
        let drain = vec![
            cost("a", Some(10), 1),
            cost("b", None, 0),
            cost("c", None, 1),
        ];
        let kept = WorkerId::new("c");
        assert_eq!(
            ids(&kept_first(drain.clone(), Some(&kept))),
            ["a", "c", "b"]
        );
        assert_eq!(ids(&kept_first(drain.clone(), None)), ["a", "b", "c"]);
        let gone = WorkerId::new("z");
        assert_eq!(ids(&kept_first(drain, Some(&gone))), ["a", "b", "c"]);
    }
}
