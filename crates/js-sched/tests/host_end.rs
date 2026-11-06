//! A worker whose host is taken back at a given time ([`Event::HostEnd`]): work is placed
//! on it only if its max wall time ends by then, its wall time is cut to the time left,
//! and a whole-machine hold on it is bounded by the end. Every time is farm time fed to
//! the scheduler; nothing waits.

#[macro_use]
#[path = "booked/mod.rs"]
mod booked;
#[allow(dead_code, reason = "each test binary uses some of the stand-in")]
#[path = "digest/mod.rs"]
mod digest;
#[allow(dead_code, reason = "each test binary uses some of the rig")]
#[path = "whole_rig/mod.rs"]
mod whole_rig;

use js_sched::{Event, Request, Scheduler};
use js_types::sched::{FarmTime, LeaseKind, OperationId};
use js_whole::WholeNodeConfig;

use crate::whole_rig::{Farm, caps, request, secs, w};

fn t(ms: u64) -> FarmTime {
    FarmTime::from_millis(ms)
}

/// A farm of the workers `names` under the rule of the suite, the host of `a` ending at
/// 100 s.
fn farm(names: &[&str]) -> Farm {
    let mut f = Farm::new(Scheduler::new(1).with_load_config(booked::current()));
    for name in names {
        f.worker(name);
    }
    end_a(&mut f, Some(100_000));
    f
}

/// Sets the end of the host of `a`, at the farm time now.
fn end_a(f: &mut Farm, ends_at: Option<u64>) {
    let effects = f.feed(Event::HostEnd {
        worker: w("a"),
        ends_at: ends_at.map(t),
    });
    assert!(effects.is_empty());
}

/// Submits an action that may run `wall_s` seconds (none: no wall time) and ticks: the
/// operation and where it was placed, if anywhere.
fn action(f: &mut Farm, wall_s: Option<u64>) -> (OperationId, Option<String>) {
    f.next_n += 1;
    let operation = f.s.next_operation();
    if let Some(wall_s) = wall_s {
        f.s.set_max_wall(operation, secs(wall_s));
    }
    f.submit(request(f.next_n, LeaseKind::Action));
    let placed = f.grants().pop().map(|g| g.worker.as_str().to_owned());
    (operation, placed)
}

both_rules! {
/// Asserts that a job whose max wall time runs one second past the end of the host is not
/// placed there, and is placed on a worker with no end when there is one. Catches: the end
/// ignored, as before the change, when the job of 101 s is placed on `a`.
fn a_job_that_would_outlast_the_host_is_not_placed_on_it() {
    let mut f = farm(&["a"]);
    let (operation, placed) = action(&mut f, Some(101));
    assert_eq!(placed, None);
    assert_eq!(f.s.max_wall(operation), Some(secs(101)));
    f.worker("b");
    f.beat();
    assert_eq!(f.grants().pop().map(|g| g.worker), Some(w("b")));
}
}

both_rules! {
/// Asserts that a job whose max wall time ends exactly at the end of the host is placed
/// there and keeps its wall time. Catches: the boundary taken as too late.
fn a_job_ending_exactly_at_the_end_of_the_host_is_placed() {
    let mut f = farm(&["a"]);
    let (operation, placed) = action(&mut f, Some(100));
    assert_eq!(placed.as_deref(), Some("a"));
    assert_eq!(f.s.max_wall(operation), Some(secs(100)));
}
}

both_rules! {
/// Asserts that the time left counts from now: at 10 s a job of 91 s is not placed on the
/// host that ends at 100 s, one of 90 s is, and one of 40 s is placed with its own wall
/// time. Catches: the wall time compared with the end alone, not from now.
fn the_time_left_counts_from_now() {
    let mut f = farm(&["a"]);
    f.at(10_000);
    f.beat();
    assert_eq!(action(&mut f, Some(91)).1, None);
    let (long, placed) = action(&mut f, Some(90));
    assert_eq!((placed.as_deref(), f.s.max_wall(long)), (Some("a"), Some(secs(90))));
    let (short, placed) = action(&mut f, Some(40));
    assert_eq!((placed.as_deref(), f.s.max_wall(short)), (Some("a"), Some(secs(40))));
}
}

both_rules! {
/// Asserts that a job with no wall time is placed on a host with an end and gets the time
/// left as its wall time (100 s at 0 s, 90 s at 10 s), and that on a worker with no end it
/// stays without one. Catches: a job with no wall time left to run past the end.
fn a_job_with_no_wall_time_gets_the_time_left() {
    let mut f = farm(&["a"]);
    let (first, placed) = action(&mut f, None);
    assert_eq!(placed.as_deref(), Some("a"));
    assert_eq!(f.s.max_wall(first), Some(secs(100)));
    f.at(10_000);
    f.beat();
    let (second, placed) = action(&mut f, None);
    assert_eq!(placed.as_deref(), Some("a"));
    assert_eq!(f.s.max_wall(second), Some(secs(90)));
    let mut f = farm(&["b"]);
    let (third, placed) = action(&mut f, None);
    assert_eq!(placed.as_deref(), Some("b"));
    assert_eq!(f.s.max_wall(third), None);
}
}

both_rules! {
/// Asserts that an end of `None` clears the end: the job of 101 s that was not placed is
/// placed, and the host shows no end. Catches: a cleared end still held.
fn an_end_of_none_clears_the_end() {
    let mut f = farm(&["a"]);
    assert_eq!(action(&mut f, Some(101)).1, None);
    assert_eq!(f.s.host_end(&w("a")), Some(t(100_000)));
    end_a(&mut f, None);
    assert_eq!(f.s.host_end(&w("a")), None);
    assert_eq!(f.grants().len(), 1);
}
}

both_rules! {
/// Asserts that an end for a worker that never registered is ignored: no effect, no end
/// shown, and the worker has none when it registers later. Catches: an end kept for a
/// worker the scheduler does not know.
fn an_end_for_an_unknown_worker_is_ignored() {
    let mut f = farm(&["a"]);
    let effects = f.feed(Event::HostEnd {
        worker: w("ghost"),
        ends_at: Some(t(5)),
    });
    assert!(effects.is_empty());
    assert_eq!(f.s.host_end(&w("ghost")), None);
    f.worker("ghost");
    assert_eq!(f.s.host_end(&w("ghost")), None);
}
}

both_rules! {
/// Asserts that a worker's end stays through a new registration and a change of capacity.
/// Catches: the end reset when the worker registers again or sends its report again.
fn a_registration_and_a_change_of_capacity_keep_the_end() {
    let mut f = farm(&["a"]);
    f.worker("a");
    assert_eq!(f.s.host_end(&w("a")), Some(t(100_000)));
    f.feed(Event::Capacity {
        worker: w("a"),
        capacity: crate::whole_rig::CAPACITY,
        caps: caps(),
    });
    assert_eq!(f.s.host_end(&w("a")), Some(t(100_000)));
    assert_eq!(action(&mut f, Some(101)).1, None);
}
}

both_rules! {
/// Asserts that a probe pinned to the worker is placed only if its wall time ends by the
/// end of the host (cordoned, as a probe's worker is): one of 101 s stays queued, one of 100 s is placed. Catches: the
/// probe path placing a pinned request past the end.
fn a_probe_is_placed_on_its_worker_only_if_it_ends_by_the_end() {
    let mut f = farm(&["a"]);
    // A probe runs on a cordoned worker.
    assert!(f.feed(Event::Cordon { worker: w("a") }).is_empty());
    let pinned = |n: u8| Request {
        pin: Some(w("a")),
        ..request(n, LeaseKind::Action)
    };
    let late = f.s.next_operation();
    f.s.set_max_wall(late, secs(101));
    f.submit(pinned(40));
    assert!(f.grants().is_empty());
    f.next_n = 41;
    let on_time = f.s.next_operation();
    f.s.set_max_wall(on_time, secs(100));
    f.submit(pinned(41));
    let grants = f.grants();
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0].operation, on_time);
}
}

/// Whole-node settings: a drain deadline of a minute from the first moment.
fn cfg() -> WholeNodeConfig {
    WholeNodeConfig {
        drain_after_ms: 0,
        drain_deadline_ms: Some(60_000),
        reserve_backoff_ms: 10_000,
        max_reserve_attempts: 2,
        ..WholeNodeConfig::dedicated(4)
    }
}

/// A farm of the workers `names`, each with a lease that may run ten minutes from farm
/// time zero, then the host of `a` ending at 30 s.
fn busy(names: &[&str]) -> Farm {
    let s = Scheduler::new(1)
        .with_load_config(booked::current())
        .with_whole_node(cfg());
    let mut f = Farm::new(s);
    for name in names {
        f.worker(name);
        f.lease_on(name, secs(600));
    }
    end_a(&mut f, Some(30_000));
    f
}

both_rules! {
/// Asserts that the hold of a whole-machine request that may run 20 s on a host that ends
/// at 30 s is given up at 30 s, not at the drain deadline of a minute. Catches: a deadline
/// past the end of the host.
fn the_deadline_of_a_hold_is_cut_to_the_end_of_the_host() {
    let mut f = busy(&["a"]);
    let op = f.submit_whole(Some(secs(20)));
    assert!(f.grants().is_empty());
    let mark = f.s.whole_mark(op).expect("marked");
    assert_eq!((&mark.worker, mark.made, mark.deadline), (&w("a"), t(0), t(30_000)));
}
}

both_rules! {
/// Asserts that a lease's end bound is its start plus its wall time cut to the end of the
/// host: leased for ten minutes from 0 s on a host that ends at 30 s, the worker's tail,
/// which a mark records, ends at 30 s. Catches: the bound of a lease past the end of its
/// host.
fn the_end_bound_of_a_lease_is_cut_to_the_end_of_the_host() {
    let mut f = busy(&["a"]);
    let op = f.submit_whole(Some(secs(20)));
    assert!(f.grants().is_empty());
    let mark = f.s.whole_mark(op).expect("marked");
    assert_eq!(mark.tail_end, Some(t(30_000)));
}
}

both_rules! {
/// Asserts that a worker whose host ends before a whole-machine request could run is not
/// held for it: asking 40 s of a host that ends at 30 s, the request holds the other
/// worker, whose host does not end. Catches: a hold on a worker that is gone before the
/// request could finish.
fn a_host_that_ends_too_soon_is_not_held_for_a_whole_machine_request() {
    let mut f = busy(&["a", "b"]);
    let op = f.submit_whole(Some(secs(40)));
    assert!(f.grants().is_empty());
    assert_eq!(f.held_for(op), Some(&w("b")));
    assert_eq!(f.s.whole_mark(op).map(|m| m.worker.clone()), Some(w("b")));
}
}

both_rules! {
/// Asserts that with only a host that ends too soon, a whole-machine request holds
/// nothing. Catches: a hold on a worker that is gone before the request could finish.
fn a_request_no_host_can_finish_holds_nothing() {
    let mut f = busy(&["a"]);
    let op = f.submit_whole(Some(secs(40)));
    assert!(f.grants().is_empty());
    assert_eq!(f.held_for(op), None);
    assert_eq!(f.s.whole_mark(op), None);
}
}
