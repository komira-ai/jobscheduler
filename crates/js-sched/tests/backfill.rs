//! Backfill on a held worker ([`Scheduler::with_backfill`]): an operation after a
//! whole-machine request in queue order is placed on the worker the request holds only if
//! it ends, with a minute to spare, by the end of that worker's tail as the hold took it.
//! The request then starts when the tail ends. Every time is farm time fed to the
//! scheduler; an operation's end is its max wall time, the bound an executor enforces.

#[macro_use]
#[path = "booked/mod.rs"]
mod booked;
#[allow(dead_code, reason = "each test binary uses some of the stand-in")]
#[path = "digest/mod.rs"]
mod digest;

#[allow(dead_code, reason = "each test binary uses some of the rig")]
#[path = "whole_rig/mod.rs"]
mod whole_rig;

#[path = "load_curves/mod.rs"]
mod load_curves;

use std::time::Duration;

use crate::whole_rig::{Farm, request, secs, w};
use js_sched::{Backfill, Request, Scheduler};
use js_types::sched::{FarmTime, LeaseGrant, LeaseKind, OperationId, Qos, WorkerId};
use js_whole::WholeNodeConfig;

/// A ceiling no wall time in these tests reaches.
const DAY: Duration = Duration::from_secs(86_400);

/// A scheduler with whole-node settings, placing by the booked rule, with `backfill`.
fn farm(backfill: Option<Backfill>) -> Farm {
    let s = Scheduler::new(1)
        .with_load_config(booked::booked())
        .with_whole_node(WholeNodeConfig::dedicated(4));
    Farm::new(match backfill {
        Some(backfill) => s.with_backfill(backfill),
        None => s,
    })
}

/// [`farm`] with bounds enforced up to a day.
fn enforced() -> Farm {
    farm(Some(Backfill::enforced(DAY)))
}

/// Worker `a` with one lease that may run 600 s, from 0, and a whole-machine request that
/// holds `a`: its tail ends at 600 s. The lease and the request.
fn marked(f: &mut Farm) -> (LeaseGrant, OperationId) {
    f.worker("a");
    let lease = f.lease_on("a", secs(600));
    let whole = f.whole();
    assert_eq!(f.held_for(whole), Some(&w("a")));
    (lease, whole)
}

/// Submits an action of `qos` that may run `wall` (none: no wall time) at the farm time
/// now and runs a round: its grant, started, if it was placed.
fn job_of(f: &mut Farm, qos: Qos, wall: Option<Duration>) -> (OperationId, Option<LeaseGrant>) {
    f.next_n += 1;
    let operation = f.s.next_operation();
    if let Some(wall) = wall {
        f.s.set_max_wall(operation, wall);
    }
    f.submit(Request {
        qos,
        ..request(f.next_n, LeaseKind::Action)
    });
    let grant = f.grants().into_iter().find(|g| g.operation == operation);
    if let Some(grant) = &grant {
        f.start(grant);
    }
    (operation, grant)
}

/// [`job_of`] at the request's QoS, `ci`.
fn job(f: &mut Farm, wall: Option<Duration>) -> Option<WorkerId> {
    job_of(f, Qos::Ci, wall).1.map(|g| g.worker)
}

/// Catches: a held worker closed to all later work (no backfill), or open to work that
/// ends after the tail. At 10 s a job of 300 s ends at 310 s, a minute and more before
/// the tail ends at 600 s, and is placed on `a`; a job of 600 s would end at 610 s and is
/// not.
#[test]
fn a_job_ending_inside_the_tail_backfills_and_one_ending_after_does_not() {
    let mut f = enforced();
    let (_, whole) = marked(&mut f);
    f.at(10_000);
    f.beat();
    assert_eq!(job(&mut f, Some(secs(300))), Some(w("a")));
    assert_eq!(job(&mut f, Some(secs(600))), None);
    assert_eq!(f.held_for(whole), Some(&w("a")));
}

/// Catches: the margin left out, or `<` for `<=`: at 10 s the tail leaves 590 s, of which
/// a minute is kept spare, so a job of 530 s fits exactly and one of 530.001 s does not.
#[test]
fn a_job_fits_up_to_the_tail_end_less_a_minute_and_not_a_millisecond_more() {
    let mut f = enforced();
    marked(&mut f);
    f.at(10_000);
    f.beat();
    let over = Duration::from_millis(530_001);
    assert_eq!(job(&mut f, Some(over)), None);
    assert_eq!(job(&mut f, Some(secs(530))), Some(w("a")));
}

/// Catches: backfill delaying the whole-machine request past the tail it held by. A job of
/// 120 s is submitted every 100 s and each ends at its bound: those submitted at 100, 200,
/// 300 and 400 s end by 520 s and are placed on `a`; the one at 500 s would end at 620 s
/// and waits. The lease the tail was taken from ends at 600 s, and the request starts on
/// `a` then, at the end the hold took, with the job of 500 s still queued behind it.
#[test]
fn the_whole_machine_request_starts_when_the_tail_it_held_by_ends() {
    let mut f = enforced();
    let (lease, whole) = marked(&mut f);
    let mut running: Vec<(u64, LeaseGrant)> = Vec::new();
    let mut waiting = Vec::new();
    for at in [100_000, 200_000, 300_000, 400_000, 500_000] {
        for (end, grant) in std::mem::take(&mut running) {
            if end <= at {
                assert!(f.advance(end).is_empty());
                f.finish(&grant);
            } else {
                running.push((end, grant));
            }
        }
        assert!(f.advance(at).is_empty());
        match job_of(&mut f, Qos::Ci, Some(secs(120))) {
            (_, Some(grant)) => {
                assert_eq!(grant.worker, w("a"));
                running.push((at + 120_000, grant));
            }
            (op, None) => waiting.push((at, op)),
        }
    }
    assert_eq!(running.len(), 1);
    assert_eq!(waiting.len(), 1);
    assert_eq!(waiting[0].0, 500_000);
    let (end, last) = running.pop().expect("one left");
    assert!(f.advance(end).is_empty());
    f.finish(&last);
    assert!(f.advance(599_999).is_empty());
    f.at(600_000);
    f.finish(&lease);
    let grants = f.grants();
    let ops: Vec<OperationId> = grants.iter().map(|g| g.operation).collect();
    assert_eq!(ops, [whole]);
    assert_eq!(grants[0].worker, w("a"));
}

/// Catches: backfill with no executor that enforces a bound. The default places nothing on
/// a held worker, whatever the job's wall time; the same job is placed once bounds are
/// enforced (above).
#[test]
fn with_bounds_not_enforced_nothing_backfills() {
    let mut f = farm(None);
    marked(&mut f);
    f.at(10_000);
    f.beat();
    assert_eq!(job(&mut f, Some(secs(1))), None);
    let mut f = farm(Some(Backfill::default()));
    marked(&mut f);
    f.at(10_000);
    f.beat();
    assert_eq!(job(&mut f, Some(secs(1))), None);
}

/// Catches: a bound above the ceiling trusted, the ceiling itself refused, and a job with
/// no wall time taken to end at once. With a ceiling of 300 s a job of 300 s backfills, one
/// of 301 s (which would end in time) does not, and one with no wall time does not.
#[test]
fn a_bound_above_the_ceiling_or_none_never_backfills() {
    let mut f = farm(Some(Backfill::enforced(secs(300))));
    marked(&mut f);
    f.at(10_000);
    f.beat();
    assert_eq!(job(&mut f, Some(secs(301))), None);
    assert_eq!(job(&mut f, None), None);
    assert_eq!(job(&mut f, Some(secs(300))), Some(w("a")));
}

/// Catches: a held worker whose tail is unknown taken as free, or as ending now. The lease
/// on `a` has no wall time and no estimate, so the tail of `a` is unknown and no job, of
/// any length, is placed there.
#[test]
fn a_worker_with_a_lease_of_unknown_end_takes_no_backfill() {
    let mut f = enforced();
    f.worker("a");
    f.place_on("a", None);
    let whole = f.whole();
    assert_eq!(f.held_for(whole), Some(&w("a")));
    f.at(10_000);
    f.beat();
    assert_eq!(job(&mut f, Some(secs(1))), None);
}

/// Catches: an overdue lease read as gone. At 650 s the lease of 600 s still runs (its
/// executor has not ended it yet): the tail is past, so even a job of a second is not
/// placed, and the request starts only when the lease does end, at 700 s.
#[test]
fn an_overdue_lease_admits_no_backfill_and_the_request_waits_for_it() {
    let mut f = enforced();
    let (lease, whole) = marked(&mut f);
    assert!(f.advance(650_000).is_empty());
    assert_eq!(job(&mut f, Some(secs(1))), None);
    assert!(f.advance(699_000).is_empty());
    f.at(700_000);
    f.finish(&lease);
    let ops: Vec<OperationId> = f.grants().iter().map(|g| g.operation).collect();
    assert_eq!(ops, [whole]);
}

/// Catches: a hold made on an overdue worker given a start time of now or of zero wait:
/// the lease of 60 s is past its bound at 100 s when the request is made, the tail it
/// holds by is already over, and a job of a second is not placed.
#[test]
fn a_hold_made_on_an_overdue_worker_admits_no_backfill() {
    let mut f = enforced();
    f.worker("a");
    f.lease_on("a", secs(60));
    assert!(f.advance(100_000).is_empty());
    let whole = f.whole();
    assert_eq!(f.held_for(whole), Some(&w("a")));
    assert_eq!(job(&mut f, Some(secs(1))), None);
}

/// Catches: the gate applied to work ahead of the request, or not to work behind it. A
/// job of `interactive` QoS submitted after the request is ahead of it in queue order and
/// is placed on `a` though it runs an hour; a job of the request's QoS with the same wall
/// time is behind it and is not.
#[test]
fn work_ahead_of_the_request_in_queue_order_is_not_held_to_the_tail() {
    let mut f = enforced();
    marked(&mut f);
    f.at(10_000);
    f.beat();
    let (_, ahead) = job_of(&mut f, Qos::Interactive, Some(secs(3_600)));
    assert_eq!(ahead.map(|g| g.worker), Some(w("a")));
    assert_eq!(job(&mut f, Some(secs(3_600))), None);
}

/// Catches: the start time taken again every round, so work placed since extends it. A job
/// of an hour ahead of the request lands on `a`, whose tail now ends at 3_610 s; a job of
/// 1_000 s behind the request would end inside that, but not inside the 600 s the hold
/// took, and is not placed.
#[test]
fn work_placed_since_the_hold_never_moves_its_start_later() {
    let mut f = enforced();
    marked(&mut f);
    f.at(10_000);
    f.beat();
    let (_, ahead) = job_of(&mut f, Qos::Interactive, Some(secs(3_600)));
    assert!(ahead.is_some());
    f.at(20_000);
    f.beat();
    assert_eq!(job(&mut f, Some(secs(1_000))), None);
    assert_eq!(job(&mut f, Some(secs(500))), Some(w("a")));
}

/// Catches: a start time kept when the tail ends sooner. `a` carries a lease of 600 s and
/// one of 300 s; the first ends at 50 s, so the tail now ends at 300 s, and a job of
/// 400 s at 60 s (in time for 600 s, not for 300 s) is not placed.
#[test]
fn a_tail_that_ends_sooner_lowers_the_start() {
    let mut f = enforced();
    f.worker("a");
    let long = f.lease_on("a", secs(600));
    f.lease_on("a", secs(300));
    let whole = f.whole();
    assert_eq!(f.held_for(whole), Some(&w("a")));
    f.at(50_000);
    f.finish(&long);
    assert!(f.grants().is_empty());
    f.at(60_000);
    f.beat();
    assert_eq!(job(&mut f, Some(secs(400))), None);
    assert_eq!(job(&mut f, Some(secs(100))), Some(w("a")));
}

/// Catches: the gate written for whole-machine marks alone. An action that owns its worker
/// holds `a` by its tail (600 s); a job of 300 s backfills and one of 600 s does not.
#[test]
fn a_worker_held_for_an_owner_action_takes_backfill_by_its_tail() {
    let mut f = enforced();
    f.worker("a");
    f.lease_on("a", secs(600));
    f.next_n += 1;
    let owner = f.s.next_operation();
    f.submit(Request {
        whole_worker: true,
        ..request(f.next_n, LeaseKind::Action)
    });
    assert!(f.grants().is_empty());
    assert_eq!(f.held_for(owner), Some(&w("a")));
    f.at(10_000);
    f.beat();
    assert_eq!(job(&mut f, Some(secs(300))), Some(w("a")));
    assert_eq!(job(&mut f, Some(secs(600))), None);
}

/// Catches: a hold made without whole-node settings given a start time. Without them the
/// hold has none, so even with bounds enforced nothing backfills there.
#[test]
fn without_whole_node_settings_a_hold_takes_no_backfill() {
    let s = Scheduler::new(1)
        .with_load_config(booked::booked())
        .with_backfill(Backfill::enforced(DAY));
    let mut f = Farm::new(s);
    marked(&mut f);
    f.at(10_000);
    f.beat();
    assert_eq!(job(&mut f, Some(secs(1))), None);
}

/// Catches: a worker held outside the scheduler ([`Scheduler::reserve_outside`]) open to
/// all work, closed to backfill, or held by a request of the scheduler. `a` is held until
/// 600 s: a job of 300 s at 10 s is placed there and one of 600 s is not; the scheduler's
/// own whole-machine request holds `b` (whose lease has no known end, so it takes no
/// backfill), not `a`; and a worker held with no start time known takes nothing.
#[test]
fn a_worker_held_outside_takes_only_backfill_by_its_start() {
    let mut f = enforced();
    f.worker("a");
    f.worker("b");
    f.lease_on("a", secs(600));
    f.place_on("b", None);
    f.s.reserve_outside(w("a"), Some(FarmTime::from_millis(600_000)));
    let whole = f.whole();
    assert_eq!(f.held_for(whole), Some(&w("b")));
    f.at(10_000);
    f.beat();
    assert_eq!(job(&mut f, Some(secs(300))), Some(w("a")));
    assert_eq!(job(&mut f, Some(secs(600))), None);
    f.s.reserve_outside(w("a"), None);
    assert_eq!(job(&mut f, Some(secs(1))), None);
}

/// Catches: a probe let past the outside hold of its worker. A probe pinned to `a` (held
/// outside until 600 s, and cordoned, as a probe's worker is) that runs 600 s is not
/// placed; one of 300 s is.
#[test]
fn a_probe_on_a_worker_held_outside_is_placed_only_as_backfill() {
    let mut f = enforced();
    f.worker("a");
    f.lease_on("a", secs(600));
    f.s.reserve_outside(w("a"), Some(FarmTime::from_millis(600_000)));
    f.feed(js_sched::Event::Cordon { worker: w("a") });
    f.at(10_000);
    f.beat();
    let probe = |f: &mut Farm, wall: Duration| {
        f.next_n += 1;
        let operation = f.s.next_operation();
        f.s.set_max_wall(operation, wall);
        f.submit(Request {
            pin: Some(w("a")),
            ..request(f.next_n, LeaseKind::Action)
        });
        f.grants().into_iter().find(|g| g.operation == operation)
    };
    assert!(probe(&mut f, secs(600)).is_none());
    assert_eq!(probe(&mut f, secs(300)).map(|g| g.worker), Some(w("a")));
}

/// Catches: a job that fits the tail placed on the held worker whatever its room or
/// platform. At 10 s a job of 300 s fits `a`'s tail, but one asking 7.5 of `a`'s 8 cores
/// (one is booked) has no room there and one that needs macOS does not match `a`; neither
/// is placed. The same job with one core and no platform is.
#[test]
fn a_job_that_fits_the_tail_still_needs_room_and_a_platform_match() {
    let mut f = enforced();
    marked(&mut f);
    f.at(10_000);
    f.beat();
    let ask = |f: &mut Farm, change: &dyn Fn(&mut Request)| {
        f.next_n += 1;
        let operation = f.s.next_operation();
        f.s.set_max_wall(operation, secs(300));
        let mut job = request(f.next_n, LeaseKind::Action);
        change(&mut job);
        f.submit(job);
        f.grants().into_iter().find(|g| g.operation == operation)
    };
    let big =
        |r: &mut Request| r.resources = js_types::sched::Resources::new(7_500, whole_rig::GIB);
    assert!(ask(&mut f, &big).is_none());
    let mac = |r: &mut Request| {
        r.needs = js_caps::Request::from_platform([("OSFamily", "macos")]).unwrap();
    };
    assert!(ask(&mut f, &mac).is_none());
    let plain = ask(&mut f, &|_| {});
    assert_eq!(plain.map(|g| g.worker), Some(w("a")));
}

/// Catches: an action that owns its worker delayed past its start by backfill. It holds
/// `a`, whose lease ends at 600 s; a job of 120 s is submitted every 100 s, in time order,
/// and each ends at its bound, so two run at once on `a` at times: those at 100, 200, 300
/// and 400 s end with a minute to spare and run there, the one at 500 s would end at
/// 620 s and waits.
/// The owner action is placed on `a` at 600 s, when the lease ends, ahead of the waiting
/// job.
#[test]
fn an_owner_action_starts_at_its_tail_through_a_stream_of_backfill() {
    let mut f = enforced();
    f.worker("a");
    let lease = f.lease_on("a", secs(600));
    f.next_n += 1;
    let owner = f.s.next_operation();
    f.submit(Request {
        whole_worker: true,
        ..request(f.next_n, LeaseKind::Action)
    });
    assert!(f.grants().is_empty());
    assert_eq!(f.held_for(owner), Some(&w("a")));
    // Submissions, and the ends of what runs, in time order.
    let mut running: Vec<(u64, LeaseGrant)> = Vec::new();
    let mut placed = Vec::new();
    for at in [100_000, 200_000, 300_000, 400_000, 500_000] {
        running.sort_by_key(|(end, _)| *end);
        while running.first().is_some_and(|(end, _)| *end <= at) {
            let (end, grant) = running.remove(0);
            assert!(f.advance(end).is_empty());
            f.finish(&grant);
        }
        assert!(f.advance(at).is_empty());
        if let (_, Some(grant)) = job_of(&mut f, Qos::Ci, Some(secs(120))) {
            assert_eq!(grant.worker, w("a"));
            placed.push(at);
            running.push((at + 120_000, grant));
        }
    }
    assert_eq!(placed, [100_000, 200_000, 300_000, 400_000]);
    assert_eq!(running.len(), 1);
    running.sort_by_key(|(end, _)| *end);
    for (end, grant) in running {
        assert!(f.advance(end).is_empty());
        f.finish(&grant);
    }
    assert!(f.advance(599_000).is_empty());
    f.at(600_000);
    f.finish(&lease);
    let ops: Vec<OperationId> = f.grants().iter().map(|g| g.operation).collect();
    assert_eq!(ops, [owner]);
}

/// Catches: a hold traded for the worker that became cheaper to drain, or its start time
/// taken again. `a` carries a lease of 600 s and `b` one of 900 s, so the request holds `a`
/// with its start at 600 s. A job of an hour ahead of the request in queue order lands on
/// `a`, whose tail is now an hour away; the hold stays on `a` with its start of 600 s. At
/// 20 s a job of 700 s, which does not fit that start, is placed on `b`.
#[test]
fn a_hold_stays_on_its_worker_with_its_start_when_work_ahead_lands_there() {
    let mut f = enforced();
    f.worker("a");
    f.worker("b");
    f.lease_on("a", secs(600));
    f.lease_on("b", secs(900));
    let whole = f.whole();
    assert_eq!(f.held_for(whole), Some(&w("a")));
    f.at(10_000);
    f.beat();
    let (_, ahead) = job_of(&mut f, Qos::Interactive, Some(secs(3_600)));
    assert_eq!(ahead.map(|g| g.worker), Some(w("a")));
    assert!(f.grants().is_empty());
    assert_eq!(f.held_for(whole), Some(&w("a")));
    f.at(20_000);
    f.beat();
    assert_eq!(job(&mut f, Some(secs(700))), Some(w("b")));
    assert_eq!(f.held_for(whole), Some(&w("a")));
}

/// Catches: a moved hold keeping the start time of the worker it left, or the worker it
/// left still held. `a` carries a lease of 600 s and `b` one of 900 s, so the request
/// holds `a` with its start at 600 s. `a` is cordoned at 10 s, so it can no longer hold
/// the request: the hold moves to `b` and takes `b`'s start, 900 s. At 20 s a job of 700 s
/// fits that start (but not 600 s) and is placed on `b`. Once `a` is uncordoned, a job of
/// an hour, which does not fit before `b`'s start of 900 s, is placed on `a`: nothing holds
/// it any more, and the hold stays on `b`.
#[test]
fn a_moved_hold_takes_its_new_workers_start_and_frees_the_old_one() {
    let mut f = enforced();
    f.worker("a");
    f.worker("b");
    f.lease_on("a", secs(600));
    f.lease_on("b", secs(900));
    let whole = f.whole();
    assert_eq!(f.held_for(whole), Some(&w("a")));
    f.at(10_000);
    f.feed(js_sched::Event::Cordon { worker: w("a") });
    f.beat();
    assert!(f.grants().is_empty());
    assert_eq!(f.held_for(whole), Some(&w("b")));
    f.at(20_000);
    f.beat();
    assert_eq!(job(&mut f, Some(secs(700))), Some(w("b")));
    f.feed(js_sched::Event::Uncordon { worker: w("a") });
    f.at(30_000);
    f.beat();
    assert_eq!(job(&mut f, Some(secs(3_600))), Some(w("a")));
    assert_eq!(f.held_for(whole), Some(&w("b")));
}

/// Catches: an underestimated tail read as still to come, or as a free worker. Every key's
/// P90 is a minute: `a`'s lease may run an hour but is estimated to end at 60 s, which is
/// the tail the request holds `a` by. At 30 s a job (a minute by its P90) does not fit; at
/// 90 s the lease still runs past its estimate, the tail is past, and not even a job of a
/// second is placed, nor is the request; it is placed when the lease ends, at 200 s.
#[test]
fn a_lease_that_outlives_its_p90_stops_backfill_and_the_request_waits_for_it() {
    let curves = load_curves::Curves::new("main");
    curves.set_life_all(1);
    let s = Scheduler::new(1)
        .with_load_config(booked::booked())
        .with_whole_node(WholeNodeConfig::dedicated(4))
        .with_curves(curves)
        .with_backfill(Backfill::enforced(DAY));
    let mut f = Farm::new(s);
    f.worker("a");
    let lease = f.lease_on("a", secs(3_600));
    let whole = f.whole();
    assert_eq!(f.held_for(whole), Some(&w("a")));
    f.at(30_000);
    f.beat();
    assert_eq!(job(&mut f, Some(secs(3_600))), None);
    assert!(f.advance(90_000).is_empty());
    assert_eq!(job(&mut f, Some(secs(1))), None);
    assert!(f.advance(199_000).is_empty());
    f.at(200_000);
    f.finish(&lease);
    let ops: Vec<OperationId> = f.grants().iter().map(|g| g.operation).collect();
    assert_eq!(ops, [whole]);
}

/// A curve source that knows the P90 of one key alone, and no curve.
struct OneP90(js_types::sched::WorkKey, u32);

impl js_types::sched::usage::CurveSource for OneP90 {
    fn curve(&self, _: &js_types::sched::WorkKey) -> Option<js_types::sched::usage::Curve> {
        None
    }

    fn wall_minutes_p90(&self, key: &js_types::sched::WorkKey) -> Option<u32> {
        (*key == self.0).then_some(self.1)
    }
}

/// Catches: backfill that outlives its estimate let more in, or the request started on a
/// busy worker. The request holds `a` until 600 s; at 10 s a job whose key's P90 is a
/// minute, though it may run an hour, is admitted (it ends at 70 s by its P90). It runs on
/// past that and past 600 s, when the lease the tail was made of ends: from then the tail
/// is past, a job of a second is not placed, and the request waits until the job ends at
/// 900 s, then starts on `a`.
#[test]
fn backfill_that_outlives_its_p90_admits_no_more_and_the_request_waits_for_it() {
    // In `marked`, the lease is the first request and the whole-machine one the second.
    let third = request(3, LeaseKind::Action).key;
    let s = Scheduler::new(1)
        .with_load_config(booked::booked())
        .with_whole_node(WholeNodeConfig::dedicated(4))
        .with_curves(std::sync::Arc::new(OneP90(third, 1)))
        .with_backfill(Backfill::enforced(DAY));
    let mut f = Farm::new(s);
    let (lease, whole) = marked(&mut f);
    f.at(10_000);
    f.beat();
    let (_, long) = job_of(&mut f, Qos::Ci, Some(secs(3_600)));
    let long = long.expect("admitted by its P90");
    assert_eq!(long.worker, w("a"));
    assert!(f.advance(599_000).is_empty());
    f.at(600_000);
    f.finish(&lease);
    assert!(f.grants().is_empty());
    assert!(f.advance(650_000).is_empty());
    assert_eq!(job(&mut f, Some(secs(1))), None);
    assert!(f.advance(899_000).is_empty());
    f.at(900_000);
    f.finish(&long);
    let ops: Vec<OperationId> = f.grants().iter().map(|g| g.operation).collect();
    assert_eq!(ops, [whole]);
}
