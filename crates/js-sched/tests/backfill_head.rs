//! Backfill on a worker held for a blocked head, and on a held worker under placement by
//! load: work after the holder in queue order is placed there only if it ends, with a
//! minute to spare, by the holder's start time; its end reads the P90 of its key's wall
//! minutes, cut to its max wall time. Every time is farm time fed to the scheduler.

#[path = "load_curves/mod.rs"]
mod load_curves;
#[path = "load_rig/mod.rs"]
mod load_rig;

use std::time::Duration;

use js_sched::{Backfill, Event, LoadConfig, Request, Scheduler};
use js_types::sched::{LeaseKind, OperationId, Resources};
use js_whole::WholeNodeConfig;

use load_curves::Curves;
use load_rig::{GIB, Rig, w, warm};

const DAY: Duration = Duration::from_secs(86_400);

fn node() -> Resources {
    Resources::new(8_000, 32 * GIB)
}

/// `r` with `whole_node` settings (if any) and bounds enforced up to a day, keeping what
/// the scheduler was given.
fn backfilling(mut r: Rig, whole_node: bool) -> Rig {
    let given = std::mem::replace(&mut r.s, Scheduler::new(1));
    let given = given.with_backfill(Backfill::enforced(DAY));
    r.s = match whole_node {
        true => given.with_whole_node(WholeNodeConfig::dedicated(4)),
        false => given,
    };
    r
}

/// Places four actions of a core on `worker`, each charged a core, with `walls` as their
/// max wall times in seconds, the other workers cordoned for the round: their starts, in
/// the order of `walls`.
fn place_four(
    r: &mut Rig,
    worker: &str,
    others: &[&str],
    walls: [u64; 4],
) -> Vec<js_types::sched::StartLease> {
    for other in others {
        r.feed(Event::Cordon { worker: w(other) });
    }
    let ops: Vec<OperationId> = (0..4)
        .map(|_| r.action(1_000, 4 * GIB, Some(warm(1_000, 0))))
        .collect();
    for (op, wall) in ops.iter().zip(walls) {
        r.s.set_max_wall(*op, Duration::from_secs(wall));
    }
    let mut starts = r.tick();
    assert_eq!(starts.len(), 4);
    for other in others {
        r.feed(Event::Uncordon { worker: w(other) });
    }
    assert_eq!(r.held(worker).len(), 4);
    starts.sort_by_key(|s| ops.iter().position(|op| *op == s.operation));
    starts
}

/// Workers `a` (four leases ending at 200 s) and `b` (two ending at 100 s, two at 900 s),
/// and a head needing 6 cores that holds `b`: where they free up first, at 100 s, or the
/// only worker left when `a` is cordoned `early`, before the head. `a` is cordoned after,
/// if not before, so later work can go to `b` or nowhere. The head, and `b`'s four starts
/// (the two of 100 s first).
fn head_on_b(r: &mut Rig, early: bool) -> (OperationId, Vec<js_types::sched::StartLease>) {
    r.up("a", node());
    r.up("b", node());
    r.baseline("a");
    r.baseline("b");
    place_four(r, "a", &["b"], [200; 4]);
    let on_b = place_four(r, "b", &["a"], [100, 100, 900, 900]);
    if early {
        r.feed(Event::Cordon { worker: w("a") });
    }
    let head = r.action(6_000, 4 * GIB, Some(warm(6_000, 0)));
    r.round(1_000);
    assert_eq!(r.s.head_hold(head), Some(&w("b")));
    if !early {
        r.feed(Event::Cordon { worker: w("a") });
    }
    (head, on_b)
}

/// Submits an action of a core that may run `wall_s`, and runs a round at the time now:
/// the worker it was placed on, if any.
fn small(r: &mut Rig, wall_s: u64) -> Option<String> {
    let op = r.action(1_000, GIB, Some(warm(1_000, 0)));
    r.s.set_max_wall(op, Duration::from_secs(wall_s));
    let starts = r.tick();
    let started = starts.into_iter().find(|s| s.operation == op);
    started.map(|s| s.worker.as_str().to_owned())
}

/// Catches: a head's hold closed to all later work, or open to work that ends after the
/// head's start time. The head holds `b` from 1 s and is expected to start there at
/// 100 s: a job of 40 s would end at 41 s, less than a minute before, and is not placed;
/// a job of 30 s ends at 31 s, a minute before, and is placed on `b`. The head keeps its
/// hold.
#[test]
fn a_job_ending_before_the_heads_start_backfills_and_one_ending_after_does_not() {
    let mut r = backfilling(Rig::on(), true);
    let (head, _) = head_on_b(&mut r, false);
    assert_eq!(small(&mut r, 40), None);
    assert_eq!(small(&mut r, 30).as_deref(), Some("b"));
    assert_eq!(r.s.head_hold(head), Some(&w("b")));
}

/// Catches: a head's hold given a start time without whole-node settings: it is held by
/// the binding ratio, with no start time, and nothing backfills there.
#[test]
fn without_whole_node_settings_a_heads_hold_takes_no_backfill() {
    let mut r = backfilling(Rig::on(), false);
    let (head, _) = head_on_b(&mut r, true);
    assert_eq!(small(&mut r, 30), None);
    assert_eq!(r.s.head_hold(head), Some(&w("b")));
}

/// Catches: the backfill estimate read from the charge's expected life (the P90 plus
/// `expected_life_delta_minutes`, two minutes by default) or from the max wall time alone
/// instead of the raw P90. An action that owns its worker holds `a`, whose one lease ends
/// at 600 s. Jobs of a key whose P90 is five minutes, each allowed an hour: at 240 s one
/// ends at 540 s, a minute before 600 s, and is placed on `a`; at 240.001 s another would
/// end a millisecond too late and is not. By the life, or by the hour, neither would fit.
#[test]
fn the_estimate_of_a_job_is_the_p90_of_its_key_not_its_life() {
    let curves = Curves::new("load");
    curves.set_life("five", 5);
    let mut r = backfilling(Rig::with_curves(LoadConfig::default(), curves), true);
    r.up("a", node());
    r.baseline("a");
    let first = r.action(1_000, GIB, Some(warm(1_000, 0)));
    r.s.set_max_wall(first, Duration::from_secs(600));
    assert_eq!(r.tick().len(), 1);
    let request = Request {
        whole_worker: true,
        ..r.request(LeaseKind::Action, 1_000, GIB)
    };
    let owner = r.submit(request);
    assert!(r.tick().is_empty());
    assert_eq!(r.s.reservation(owner), Some(&w("a")));
    let keyed = |r: &mut Rig, at: u64| {
        r.ms = at;
        r.beat("a", None);
        let op = r.keyed("five", 1_000, GIB, warm(1_000, 0));
        r.s.set_max_wall(op, Duration::from_secs(3_600));
        let starts = r.tick();
        starts.into_iter().any(|s| s.operation == op)
    };
    assert!(keyed(&mut r, 240_000));
    assert!(!keyed(&mut r, 240_001));
}

/// Catches: a job that fits the head's start placed whatever the load. At 1 s a job of
/// 30 s fits the start (100 s), but one charged 5 cores would take `b` (4 cores charged)
/// past its 8.8 and is refused by the predicate; a job of a core is placed.
#[test]
fn a_job_that_fits_the_heads_start_is_still_held_to_the_load() {
    let mut r = backfilling(Rig::on(), true);
    head_on_b(&mut r, false);
    let heavy = r.action(5_000, GIB, Some(warm(5_000, 0)));
    r.s.set_max_wall(heavy, Duration::from_secs(30));
    assert!(r.tick().iter().all(|s| s.operation != heavy));
    assert_eq!(small(&mut r, 30).as_deref(), Some("b"));
}

/// Catches: a head delayed past its start by backfill. The head holds `b` until 100 s;
/// jobs of 30 s at 1, 5 and 9 s end by 39 s and are placed on `b`, one at 11 s would end at
/// 41 s and is not; each ends at its bound, and at 100 s, when `b`'s two leases of 100 s
/// end, the head starts on `b`.
#[test]
fn a_head_starts_at_its_start_through_a_stream_of_backfill() {
    let mut r = backfilling(Rig::on(), true);
    let (head, on_b) = head_on_b(&mut r, false);
    let mut ran = Vec::new();
    for at in [1_000, 5_000, 9_000] {
        r.ms = at;
        let op = r.action(1_000, GIB, Some(warm(1_000, 0)));
        r.s.set_max_wall(op, Duration::from_secs(30));
        let start = r
            .tick()
            .into_iter()
            .find(|s| s.operation == op)
            .expect("backfilled");
        assert_eq!(start.worker, w("b"));
        ran.push(start);
    }
    r.ms = 11_000;
    assert_eq!(small(&mut r, 30), None);
    for (start, end) in ran.iter().zip([31_000, 35_000, 39_000]) {
        r.ms = end;
        r.beat("a", None);
        r.beat("b", None);
        r.finish(start);
        assert!(r.tick().iter().all(|s| s.operation != head));
    }
    r.ms = 100_000;
    r.beat("a", None);
    r.beat("b", None);
    for start in &on_b[..2] {
        r.finish(start);
    }
    let started = r.tick().into_iter().find(|s| s.operation == head);
    assert_eq!(started.map(|s| s.worker), Some(w("b")));
}
