//! Which worker a blocked head of the queue holds: the one it can start on first. The
//! start time of a partial request is the first instant at which its charge fits against
//! the predicate once the leases on the worker are released at their ends (the lesser of
//! the estimate and the max wall time); the measured load stays as it is. Every time is
//! farm time fed to the scheduler.

#[path = "load_curves/mod.rs"]
mod load_curves;
#[path = "load_rig/mod.rs"]
mod load_rig;

use std::time::Duration;

use js_sched::{Event, LoadConfig, Request};
use js_types::sched::{LeaseKind, OperationId};

use load_curves::Curves;
use load_rig::{GIB, Rig, w, warm};

fn node() -> js_types::sched::Resources {
    js_types::sched::Resources::new(8_000, 32 * GIB)
}

/// Places four actions on `worker` alone: each books `booked` millicores and is predicted
/// at `charge`.
fn place_four(r: &mut Rig, worker: &str, others: &[&str], booked: u64, charge: u64) {
    for other in others {
        r.feed(Event::Cordon { worker: w(other) });
    }
    for _ in 0..4 {
        r.action(booked, 4 * GIB, Some(warm(charge, 0)));
    }
    assert_eq!(r.tick().len(), 4);
    for other in others {
        r.feed(Event::Uncordon { worker: w(other) });
    }
    assert_eq!(r.held(worker).len(), 4);
}

/// `r` with whole-node settings on, keeping what the scheduler was given.
fn configured(mut r: Rig) -> Rig {
    let off = std::mem::replace(&mut r.s, js_sched::Scheduler::new(1));
    r.s = off.with_whole_node(js_whole::WholeNodeConfig::dedicated(4));
    r
}

/// Two 8-core workers, each with four leases (`a`'s operations are 0 to 3, `b`'s 4 to 7),
/// charged by prediction, with whole-node settings on: the head is held by start time.
fn two_loaded(curves: Option<std::sync::Arc<Curves>>, booked: u64, charge: u64) -> Rig {
    let r = match curves {
        Some(curves) => Rig::with_curves(LoadConfig::default(), curves),
        None => Rig::on(),
    };
    fill(configured(r), booked, charge)
}

/// As [`two_loaded`], with no whole-node settings: the head is held as it always was.
fn two_loaded_off(booked: u64, charge: u64) -> Rig {
    fill(Rig::on(), booked, charge)
}

fn fill(mut r: Rig, booked: u64, charge: u64) -> Rig {
    r.up("a", node());
    r.up("b", node());
    r.baseline("a");
    r.baseline("b");
    place_four(&mut r, "a", &["b"], booked, charge);
    place_four(&mut r, "b", &["a"], booked, charge);
    r
}

/// The max wall time, in seconds, of the four operations from `first`; `None` sets none.
fn walls(r: &mut Rig, first: u64, secs: [Option<u64>; 4]) {
    for (i, wall) in (0..).zip(secs) {
        if let Some(wall) = wall {
            r.s.set_max_wall(OperationId(first + i), Duration::from_secs(wall));
        }
    }
}

fn head(r: &mut Rig, cpu: u64) -> OperationId {
    r.action(cpu, 4 * GIB, Some(warm(cpu, 0)))
}

/// Catches: the hold chosen by the load ratio, or by when the worker empties, instead of
/// by when the head's 6 cores free up: `a` empties first (all four leases end at 200 s)
/// but frees 6 cores only then; `b` has two leases ending at 100 s, which is enough, and
/// two at 900 s. The head holds `b`.
#[test]
fn the_head_holds_the_worker_where_its_cores_free_up_first() {
    let mut r = two_loaded(None, 1_000, 1_000);
    walls(&mut r, 0, [Some(200); 4]);
    walls(&mut r, 4, [Some(100), Some(100), Some(900), Some(900)]);
    let op = head(&mut r, 6_000);
    r.round(1_000);
    assert_eq!(r.s.head_hold(op), Some(&w("b")));
}

/// Catches: the start time read from the bookings: the leases book a quarter core each but
/// are charged two cores, so the head's 3 cores are refused by the load on both workers
/// though the booked capacity is free. `a` frees two cores at 100 s and the second only at
/// 900 s, `b` frees them by 500 s: the head holds `b`.
#[test]
fn the_start_time_follows_what_is_charged_not_what_is_booked() {
    let mut r = two_loaded(None, 250, 2_000);
    walls(&mut r, 0, [Some(100), Some(900), Some(900), Some(900)]);
    walls(&mut r, 4, [Some(500); 4]);
    let op = head(&mut r, 3_000);
    r.round(1_000);
    assert_eq!(r.s.head_hold(op), Some(&w("b")));
}

/// Catches: a lease with no end read as released, or as never needed: every lease on `a`
/// has neither an estimate nor a max wall time, so `a` has no start time and ranks after
/// `b`, which has one.
#[test]
fn a_worker_whose_release_is_unknown_ranks_last() {
    let mut r = two_loaded(None, 1_000, 1_000);
    walls(&mut r, 4, [Some(100), Some(100), Some(900), Some(900)]);
    let op = head(&mut r, 6_000);
    r.round(1_000);
    assert_eq!(r.s.head_hold(op), Some(&w("b")));
}

/// Catches: the load ratio dropped when nothing is known: with no end known anywhere the
/// two workers are equal and the head holds the first by name, as it did.
#[test]
fn with_no_end_known_the_hold_falls_back_to_the_load_order() {
    let mut r = two_loaded(None, 1_000, 1_000);
    let op = head(&mut r, 6_000);
    r.round(1_000);
    assert_eq!(r.s.head_hold(op), Some(&w("a")));
}

/// Catches: an overdue lease kept charged: at 600 s every lease on `b` is past its
/// estimate of a minute, so they count as released now and `b` starts the head at once,
/// while `a`'s leases (700 s) free their cores later.
#[test]
fn an_overdue_lease_counts_as_released_now() {
    let curves = Curves::new("load");
    for n in 4..8 {
        curves.set_life(&format!("k{n}"), 1);
    }
    let mut r = two_loaded(Some(curves), 1_000, 1_000);
    walls(&mut r, 0, [Some(700); 4]);
    walls(&mut r, 4, [Some(900); 4]);
    let op = head(&mut r, 6_000);
    r.ms = 600_000;
    r.beat("a", None);
    r.beat("b", None);
    r.tick();
    assert_eq!(r.s.head_hold(op), Some(&w("b")));
}

/// Catches: the measured load released with the leases: `a` is measured at 7 cores with
/// small leases, so whatever ends, the head's 3 cores never fit and `a` has no start time;
/// `b` is charged for its leases and frees enough at 100 s. The head holds `b`, though
/// the load ratio of `a` is the smaller.
#[test]
fn a_worker_bound_by_measured_load_has_no_start_time() {
    let mut r = configured(Rig::on());
    r.up("a", node());
    r.up("b", node());
    r.baseline("a");
    r.baseline("b");
    place_four(&mut r, "a", &["b"], 1_000, 250);
    place_four(&mut r, "b", &["a"], 1_000, 2_000);
    let held_a = r.held("a");
    for _ in 0..2 {
        r.window("a", 7_000, 0, &held_a);
    }
    walls(&mut r, 0, [Some(100); 4]);
    walls(&mut r, 4, [Some(100), Some(100), Some(900), Some(900)]);
    let op = head(&mut r, 3_000);
    r.round(0);
    assert_eq!(r.s.head_hold(op), Some(&w("b")));
}

/// A request that owns its worker, queued behind nothing.
fn owner(r: &mut Rig) -> OperationId {
    let request = Request {
        whole_worker: true,
        ..r.request(LeaseKind::Action, 1_000, 4 * GIB)
    };
    r.submit(request)
}

/// Catches (with whole-node settings): an owner-worker action and a blocked head both
/// holding a worker of one pool: the owner-worker action, earlier in queue order, holds;
/// the head holds nothing.
#[test]
fn with_settings_an_owner_action_ahead_of_a_head_takes_the_pools_hold() {
    let mut r = two_loaded(None, 1_000, 1_000);
    let own = owner(&mut r);
    let op = head(&mut r, 6_000);
    r.round(1_000);
    assert!(r.s.reservation(own).is_some());
    assert_eq!(r.s.head_hold(op), None);
}

/// Catches (with whole-node settings): as above in the other order: the head, earlier,
/// holds; the owner-worker action holds nothing.
#[test]
fn with_settings_a_head_ahead_of_an_owner_action_takes_the_pools_hold() {
    let mut r = two_loaded(None, 1_000, 1_000);
    let op = head(&mut r, 6_000);
    let own = owner(&mut r);
    r.round(1_000);
    assert!(r.s.head_hold(op).is_some());
    assert_eq!(r.s.reservation(own), None);
}

/// Catches (off is main): a shared slot without settings: an owner-worker action and a
/// blocked head each hold a worker, as before.
#[test]
fn without_settings_an_owner_action_and_a_head_each_hold_a_worker() {
    let mut r = two_loaded_off(1_000, 1_000);
    let own = owner(&mut r);
    let op = head(&mut r, 6_000);
    r.round(1_000);
    assert!(r.s.reservation(own).is_some());
    assert!(r.s.head_hold(op).is_some());
}

/// Eight actions of 4 cores each, placed on `worker` alone; their starts.
fn eight_on(r: &mut Rig, worker: &str, others: &[&str]) -> Vec<js_types::sched::StartLease> {
    for other in others {
        r.feed(Event::Cordon { worker: w(other) });
    }
    for _ in 0..8 {
        r.action(4_000, 4 * GIB, Some(warm(4_000, 0)));
    }
    let starts = r.tick();
    assert_eq!(starts.len(), 8);
    for other in others {
        r.feed(Event::Uncordon { worker: w(other) });
    }
    assert!(starts.iter().all(|s| s.worker == w(worker)));
    starts
}

/// Catches (the #169 shape, with whole-node settings): a head needing 24 of 32 cores held
/// by when its cores free up, and not starved. Both 32-core workers hold eight 4-core
/// leases; `a` empties first (all end at 200 s) but `b` frees 24 cores at 100 s, when six
/// of its eight end. The head holds `b`, and starts there as the six end, ahead of `a`,
/// which empties earlier than `b` does.
#[test]
fn with_settings_a_head_needing_24_of_32_cores_starts_when_they_free_up() {
    let mut r = configured(Rig::on());
    let big = js_types::sched::Resources::new(32_000, 128 * GIB);
    r.up("a", big);
    r.up("b", big);
    r.baseline("a");
    r.baseline("b");
    let on_a = eight_on(&mut r, "a", &["b"]);
    let on_b = eight_on(&mut r, "b", &["a"]);
    assert_eq!(on_a.len() + on_b.len(), 16);
    walls(&mut r, 0, [Some(200); 4]);
    walls(&mut r, 4, [Some(200); 4]);
    let wall_b = [Some(100), Some(100), Some(100), Some(100)];
    walls(&mut r, 8, wall_b);
    walls(&mut r, 12, [Some(100), Some(100), Some(900), Some(900)]);
    let op = head(&mut r, 24_000);
    r.round(1_000);
    assert_eq!(r.s.head_hold(op), Some(&w("b")));
    // At 100 s six of `b`'s leases end.
    r.ms = 100_000;
    r.beat("a", None);
    r.beat("b", None);
    for start in on_b.iter().take(6) {
        r.finish(start);
    }
    let starts = r.tick();
    let started = starts.iter().find(|s| s.operation == op);
    assert_eq!(started.map(|s| s.worker.clone()), Some(w("b")));
}
