//! The worker a blocked head of the queue holds when the host of one of its candidates ends
//! ([`Event::HostEnd`]). Every time is farm time fed to the scheduler.

#[path = "load_curves/mod.rs"]
mod load_curves;
#[path = "load_rig/mod.rs"]
mod load_rig;

use std::time::Duration;

use js_sched::{Backfill, Event, Scheduler};
use js_types::sched::{FarmTime, OperationId, Resources};
use js_whole::WholeNodeConfig;

use load_rig::{GIB, Rig, w, warm};

fn node() -> Resources {
    Resources::new(8_000, 32 * GIB)
}

/// Places four actions of a core on `worker`, each allowed `wall_s` seconds, the other
/// worker cordoned for the round.
fn place_four(r: &mut Rig, worker: &str, other: &str, wall_s: u64) {
    r.feed(Event::Cordon { worker: w(other) });
    for _ in 0..4 {
        let op = r.action(1_000, 4 * GIB, Some(warm(1_000, 0)));
        r.s.set_max_wall(op, Duration::from_secs(wall_s));
    }
    assert_eq!(r.tick().len(), 4);
    r.feed(Event::Uncordon { worker: w(other) });
    assert_eq!(r.held(worker).len(), 4);
}

/// Workers `a` and `b` with four leases each, the host of a worker ending at a number of
/// seconds when `ends` names them, and a head of 6 cores that may run 100 s, blocked by the load at
/// 1 s: the head.
fn blocked_head(whole_node: bool, ends: Option<(&str, u64)>) -> (Rig, OperationId) {
    let given = Scheduler::new(1).with_backfill(Backfill::enforced(Duration::from_secs(86_400)));
    let mut r = Rig::on();
    r.s = if whole_node {
        given.with_whole_node(WholeNodeConfig::dedicated(4))
    } else {
        given
    };
    r.up("a", node());
    r.up("b", node());
    r.baseline("a");
    r.baseline("b");
    place_four(&mut r, "a", "b", 900);
    place_four(&mut r, "b", "a", 100);
    if let Some((worker, end)) = ends {
        let ends_at = Some(FarmTime::from_millis(end * 1_000));
        r.feed(Event::HostEnd {
            worker: w(worker),
            ends_at,
        });
    }
    let head = r.action(6_000, 4 * GIB, Some(warm(6_000, 0)));
    r.s.set_max_wall(head, Duration::from_secs(100));
    r.round(1_000);
    (r, head)
}

/// Asserts that a blocked head holds the worker it would hold with no end while that host
/// has the time the head may run (100 s from 1 s), and the other worker when the host ends
/// before that, with or without whole-node settings. Catches: a head holding a worker that
/// is gone before it could finish.
#[test]
fn a_head_holds_a_worker_whose_host_lasts_not_one_that_ends_too_soon() {
    for whole_node in [true, false] {
        let (r, head) = blocked_head(whole_node, None);
        let usual = r.s.head_hold(head).cloned().expect("held");
        let other = if usual == w("a") { w("b") } else { w("a") };
        let hold = |end_s| {
            let (r, head) = blocked_head(whole_node, Some((usual.as_str(), end_s)));
            r.s.head_hold(head).cloned()
        };
        assert_eq!(hold(101), Some(usual.clone()), "{whole_node}");
        assert_eq!(hold(100), Some(other), "{whole_node}");
    }
}
