//! The view of a whole-machine request's waiting state ([`Scheduler::whole_state`]) and of
//! the caps' numbers ([`Scheduler::whole_summary`]): one test per state, each driven into
//! it and on to the next, and the summary checked against what the caps decide. Every time
//! is farm time fed to the scheduler; nothing waits.

#[path = "booked/mod.rs"]
mod booked;
#[allow(dead_code, reason = "each test binary uses some of the stand-in")]
#[path = "digest/mod.rs"]
mod digest;
#[allow(dead_code, reason = "each test binary uses some of the rig")]
#[path = "whole_rig/mod.rs"]
mod whole_rig;

use std::collections::BTreeMap;

use js_sched::{
    DaemonInstance, Event, Request, Scheduler, WholeReason, WholeState, WholeSummary, WholeWait,
};
use js_types::sched::{FarmTime, LeaseGrant, LeaseKind, NodePhase, OperationId, Resources};
use js_whole::WholeNodeConfig;
use js_whole::caps::Blocked;

use crate::whole_rig::{Farm, GIB, caps, request, secs, w};

fn t(ms: u64) -> FarmTime {
    FarmTime::from_millis(ms)
}

/// Whole-node settings: a drain deadline of a minute, a back-off of ten seconds, room
/// everywhere, and `drain_after_ms` and `attempts` as given.
fn cfg(drain_after_ms: u64, attempts: u32) -> WholeNodeConfig {
    WholeNodeConfig {
        drain_after_ms,
        drain_deadline_ms: Some(60_000),
        reserve_backoff_ms: 10_000,
        max_reserve_attempts: attempts,
        max_draining: 9,
        ..WholeNodeConfig::dedicated(9)
    }
}

/// A farm of the workers `names` with an action each that may run `wall` seconds (the
/// actions go to the least loaded worker, so each worker has one).
fn busy(config: WholeNodeConfig, names: &[(&str, u64)]) -> (Farm, Vec<LeaseGrant>) {
    let mut f = Farm::new(
        Scheduler::new(1)
            .with_load_config(booked::booked())
            .with_whole_node(config),
    );
    let grants = names
        .iter()
        .map(|(name, wall)| {
            f.worker(name);
            let grant = f.lease(secs(*wall));
            assert_eq!(grant.worker, w(name), "one action on each worker");
            grant
        })
        .collect();
    (f, grants)
}

/// Asks for a whole machine that may run ten minutes, ten seconds after the last ask,
/// and runs a round: the operation. Nothing is placed: every worker is busy.
fn ask(f: &mut Farm) -> OperationId {
    let now = f.now;
    f.advance(now + 10_000);
    let op = f.submit_whole(Some(secs(600)));
    assert!(f.grants().is_empty(), "no worker is empty");
    op
}

fn state(f: &Farm, op: OperationId) -> WholeState {
    f.s.whole_state(op).expect("a queued whole-machine request")
}

fn summary(f: &Farm) -> WholeSummary {
    f.s.whole_summary().expect("whole-node settings are on")
}

/// A round at `ms` with nothing granted.
fn tick_at(f: &mut Farm, ms: u64) {
    assert!(f.advance(ms).is_empty());
}

/// A round a second later with nothing granted.
fn next_round(f: &mut Farm) {
    let at = f.now + 1_000;
    tick_at(f, at);
}

fn holding(worker: &str, made: u64, deadline: Option<u64>, give_ups: u32) -> WholeState {
    WholeState::Holding {
        worker: w(worker),
        made: t(made),
        deadline: deadline.map(t),
        give_ups,
        draining: true,
    }
}

fn counts(of: &[(WholeReason, usize)]) -> BTreeMap<WholeReason, usize> {
    of.iter().copied().collect()
}

/// Catches: the wait before `drain_after` shown as a hold, or its end misread: the
/// request is early with the time it may first hold, a millisecond before it still is,
/// and at it holds the only worker.
#[test]
fn before_drain_after_a_request_holds_nothing_and_then_holds() {
    let (mut f, _) = busy(cfg(30_000, 2), &[("a", 600)]);
    let op = f.submit_whole(Some(secs(600)));
    assert!(f.grants().is_empty());
    assert_eq!(
        state(&f, op),
        WholeState::Early {
            may_hold_at: t(30_000)
        }
    );
    assert_eq!(f.s.whole_mark(op), None);
    tick_at(&mut f, 29_999);
    assert_eq!(
        state(&f, op),
        WholeState::Early {
            may_hold_at: t(30_000)
        }
    );
    assert_eq!(summary(&f).waiting, counts(&[(WholeReason::Early, 1)]));
    tick_at(&mut f, 30_000);
    assert_eq!(state(&f, op), holding("a", 30_000, Some(90_000), 0));
    assert_eq!(summary(&f).waiting, counts(&[(WholeReason::Holding, 1)]));
}

/// Catches: the hold's fields wrong: the worker, when it was made, the deadline and the
/// give-up count are the mark's and the counter's; the worker is draining while it has a
/// lease and no longer once the lease ended, before the round that grants it; granted,
/// the request leaves the view.
#[test]
fn a_hold_shows_its_worker_deadline_and_whether_it_is_draining_until_granted() {
    let (mut f, grants) = busy(cfg(0, 2), &[("a", 600)]);
    let op = ask(&mut f);
    assert_eq!(state(&f, op), holding("a", 10_000, Some(70_000), 0));
    let mark = f.s.whole_mark(op).cloned().expect("marked");
    assert_eq!(
        (mark.made, Some(mark.deadline)),
        (t(10_000), Some(t(70_000)))
    );
    f.finish(&grants[0]);
    let WholeState::Holding {
        worker,
        made,
        deadline,
        give_ups,
        draining,
    } = state(&f, op)
    else {
        panic!("holds");
    };
    assert_eq!(
        (worker, made, deadline, give_ups),
        (w("a"), t(10_000), Some(t(70_000)), 0)
    );
    assert!(!draining);
    let s = summary(&f);
    assert_eq!(
        (s.marks, s.draining),
        (1, 0),
        "a mark on a worker with no lease"
    );
    let granted = f.advance(20_000);
    assert_eq!(granted.len(), 1);
    assert_eq!(f.s.whole_state(op), None);
    assert_eq!(summary(&f).waiting, counts(&[]));
}

/// Catches: the back-off of a give-up not shown: the only worker is given up at its
/// deadline and barred until exactly the back-off ends, with the give-up counted; then it
/// is held again, and granted when its lease ends.
#[test]
fn a_given_up_hold_is_barred_until_the_backoff_ends_then_holds_again() {
    let (mut f, grants) = busy(cfg(0, 2), &[("a", 600)]);
    let op = ask(&mut f);
    tick_at(&mut f, 70_000);
    let barred = WholeState::Barred {
        workers: BTreeMap::from([(w("a"), t(80_000))]),
        give_ups: 1,
    };
    assert_eq!(state(&f, op), barred);
    assert_eq!(f.s.whole_give_ups(op), 1);
    assert_eq!(summary(&f).waiting, counts(&[(WholeReason::Barred, 1)]));
    tick_at(&mut f, 79_999);
    assert_eq!(state(&f, op), barred);
    tick_at(&mut f, 80_000);
    assert_eq!(state(&f, op), holding("a", 80_000, Some(140_000), 1));
    f.finish(&grants[0]);
    assert_eq!(f.advance(90_000).len(), 1);
    assert_eq!(f.s.whole_state(op), None);
}

/// Catches: the next worker and the end of give-ups not shown: given up at its deadline,
/// the request holds the other worker at once with the count raised; given up there
/// too (the attempts it has are two), it holds the first again with no deadline, as it is
/// not given up again, and is granted when that worker empties.
#[test]
fn a_given_up_request_holds_another_worker_and_at_the_cap_no_deadline() {
    let (mut f, grants) = busy(cfg(0, 2), &[("a", 600), ("b", 1_200)]);
    let op = ask(&mut f);
    assert_eq!(state(&f, op), holding("a", 10_000, Some(70_000), 0));
    tick_at(&mut f, 70_000);
    assert_eq!(state(&f, op), holding("b", 70_000, Some(130_000), 1));
    tick_at(&mut f, 130_000);
    assert_eq!(state(&f, op), holding("a", 130_000, None, 2));
    f.finish(&grants[0]);
    let granted = f.advance(140_000);
    assert_eq!(granted.len(), 1);
    assert_eq!((granted[0].operation, &granted[0].worker), (op, &w("a")));
    assert_eq!(f.s.whole_state(op), None);
}

/// Catches: a cap's stop shown as another state, or disagreeing with `whole_wait`: at one
/// whole worker allowed the second request is stopped by the ceiling, and when the first is
/// cancelled it holds the worker the first held.
#[test]
fn a_request_over_max_whole_node_is_stopped_by_the_ceiling_and_then_holds() {
    let c = WholeNodeConfig {
        max_whole_node: 1,
        ..cfg(0, 2)
    };
    let (mut f, _) = busy(c, &[("a", 600), ("b", 600)]);
    let first = ask(&mut f);
    let second = ask(&mut f);
    assert!(matches!(state(&f, first), WholeState::Holding { .. }));
    assert_eq!(
        state(&f, second),
        WholeState::Stopped(WholeWait::Cap(Blocked::Ceiling))
    );
    assert_eq!(
        f.s.whole_wait(second),
        Some(WholeWait::Cap(Blocked::Ceiling))
    );
    f.s.forget_queued(first).expect("queued");
    assert_eq!(f.s.whole_state(first), None);
    next_round(&mut f);
    assert!(matches!(state(&f, second), WholeState::Holding { .. }));
    assert_eq!(f.s.whole_wait(second), None);
}

/// Catches: `max_draining` and `keep_for_ordinary` stops lost in the view: each stops the
/// request it stops, as `whole_wait` says, and the summary counts the reason and the
/// number the cap compares.
#[test]
fn the_draining_and_keep_caps_show_as_stops_and_in_the_summary() {
    let c = WholeNodeConfig {
        max_draining: 1,
        ..cfg(0, 2)
    };
    let (mut f, _) = busy(c, &[("a", 600), ("b", 600)]);
    let first = ask(&mut f);
    let second = ask(&mut f);
    let why = WholeWait::Cap(Blocked::MaxDraining);
    assert_eq!(state(&f, second), WholeState::Stopped(why));
    assert_eq!(f.s.whole_wait(second), Some(why));
    let s = summary(&f);
    assert_eq!((s.draining, s.max_draining, s.marks), (1, 1, 1));
    assert_eq!(
        s.waiting,
        counts(&[
            (WholeReason::Holding, 1),
            (WholeReason::Cap(Blocked::MaxDraining), 1)
        ])
    );
    f.s.forget_queued(first).expect("queued");

    let c = WholeNodeConfig {
        keep_for_ordinary: 1,
        ..cfg(0, 2)
    };
    let (mut f, _) = busy(c, &[("a", 600), ("b", 600), ("c", 600)]);
    let _held: Vec<OperationId> = (0..2).map(|_| ask(&mut f)).collect();
    let third = ask(&mut f);
    let why = WholeWait::Cap(Blocked::KeepForOrdinary);
    assert_eq!(state(&f, third), WholeState::Stopped(why));
    assert_eq!(f.s.whole_wait(third), Some(why));
    let s = summary(&f);
    assert_eq!(
        (s.whole, s.ceiling, s.capable_hosts, s.keep_for_ordinary),
        (2, 2, 3, 1)
    );
}

/// A worker `a` of 16 cores with one action that ends sooner than the one `b` of 8 cores
/// has (when there is a `b`), and an ordinary action of 16 cores, which only `a` can ever
/// run, waiting ahead of the whole-machine request asked next: the farm, the request and
/// the ordinary action.
fn last_capable_farm() -> (Farm, OperationId, OperationId) {
    let mut f = Farm::new(
        Scheduler::new(1)
            .with_load_config(booked::booked())
            .with_whole_node(cfg(0, 2)),
    );
    let worker = |f: &mut Farm, name: &str, cores: u64| {
        f.feed(Event::WorkerUp {
            worker: w(name),
            instance: DaemonInstance::new(name),
            predecessor_stopped: false,
            capacity: Resources::new(cores * 1_000, 2 * cores * GIB),
            caps: caps(),
        });
        f.workers.push(w(name));
    };
    worker(&mut f, "a", 16);
    f.lease(secs(60));
    f.next_n += 1;
    let ordinary_op = f.s.next_operation();
    f.submit(Request {
        resources: Resources::new(16_000, GIB),
        ..request(f.next_n, LeaseKind::Action)
    });
    assert!(f.grants().is_empty(), "a is not free enough for it");
    let whole = ask(&mut f);
    (f, whole, ordinary_op)
}

/// Catches: the last-capable stop missing from the view: the only worker an older
/// ordinary request can run on is not held, the state says so as `whole_wait` does, and
/// once the ordinary request is gone the worker is held.
#[test]
fn the_last_capable_worker_of_an_older_request_stops_the_hold_until_it_is_gone() {
    let (mut f, whole, ordinary) = last_capable_farm();
    assert_eq!(
        state(&f, whole),
        WholeState::Stopped(WholeWait::LastCapable)
    );
    assert_eq!(f.s.whole_wait(whole), Some(WholeWait::LastCapable));
    assert_eq!(
        summary(&f).waiting,
        counts(&[(WholeReason::LastCapable, 1)])
    );
    assert_eq!(
        f.s.whole_state(ordinary),
        None,
        "not a whole-machine request"
    );
    f.s.forget_queued(ordinary).expect("queued");
    next_round(&mut f);
    assert!(matches!(state(&f, whole), WholeState::Holding { .. }));
}

/// Catches: a request whose worker is held by one ahead shown as holding or stopped: with
/// one worker the second request has nothing recorded, and is `Queued`; when the first is
/// cancelled it holds the worker.
#[test]
fn a_request_behind_another_holder_is_queued_and_then_holds() {
    let (mut f, _) = busy(cfg(0, 2), &[("a", 600)]);
    let first = ask(&mut f);
    let second = ask(&mut f);
    assert!(matches!(state(&f, first), WholeState::Holding { .. }));
    assert_eq!(state(&f, second), WholeState::Queued);
    assert_eq!(f.s.whole_wait(second), None);
    assert_eq!(
        summary(&f).waiting,
        counts(&[(WholeReason::Holding, 1), (WholeReason::Queued, 1)])
    );
    f.s.forget_queued(first).expect("queued");
    next_round(&mut f);
    assert!(matches!(state(&f, second), WholeState::Holding { .. }));
}

/// Catches: a request no worker could run shown as waiting for a cap or a hold: its state
/// is the scheduler's reason, and when a worker large enough registers it holds that one.
#[test]
fn a_request_no_worker_could_hold_is_unservable_until_one_registers() {
    let (mut f, _) = busy(cfg(0, 2), &[("a", 600)]);
    f.next_n += 1;
    let big = Request {
        resources: Resources::new(64_000, GIB),
        ..request(f.next_n, LeaseKind::WholeMachine)
    };
    let op = f.s.next_operation();
    f.submit(big);
    assert!(f.grants().is_empty());
    let reason = f.s.waiting(op).expect("no worker could run it").to_owned();
    assert_eq!(state(&f, op), WholeState::Unservable { reason });
    assert_eq!(summary(&f).waiting, counts(&[(WholeReason::Unservable, 1)]));
    f.feed(Event::WorkerUp {
        worker: w("big"),
        instance: DaemonInstance::new("big"),
        predecessor_stopped: false,
        capacity: Resources::new(64_000, 128 * GIB),
        caps: caps(),
    });
    f.workers.push(w("big"));
    let granted = f.advance(f.now + 1_000);
    assert_eq!(granted.len(), 1, "the empty large worker is taken at once");
    assert_eq!(f.s.whole_state(op), None);
}

/// Catches: a view without settings, or of a request that is not a queued whole-machine
/// one: without settings both are `None`; with them an ordinary request, an unknown
/// operation and a cancelled request have no state and are not counted.
#[test]
fn no_settings_or_no_queued_whole_machine_request_means_no_view() {
    let mut f = Farm::new(Scheduler::new(1).with_load_config(booked::booked()));
    f.worker("a");
    f.lease(secs(600));
    let op = f.submit_whole(Some(secs(600)));
    assert!(f.grants().is_empty());
    assert_eq!(f.s.whole_state(op), None);
    assert_eq!(f.s.whole_summary(), None);

    let (mut f, _) = busy(cfg(0, 2), &[("a", 600)]);
    f.next_n += 1;
    let ordinary = f.s.next_operation();
    f.submit(Request {
        resources: Resources::new(8_000, GIB),
        ..request(f.next_n, LeaseKind::Action)
    });
    assert!(f.grants().is_empty());
    assert_eq!(f.s.whole_state(ordinary), None);
    assert_eq!(f.s.whole_state(OperationId(9_999)), None);
    assert_eq!(summary(&f).waiting, counts(&[]));
    let whole = ask(&mut f);
    assert!(f.s.whole_state(whole).is_some());
    f.s.forget_queued(whole).expect("queued");
    assert_eq!(f.s.whole_state(whole), None);
    let s = summary(&f);
    assert_eq!((s.marks, s.whole), (0, 0));
    assert_eq!(s.waiting, counts(&[]));
}

/// Catches: the summary drifting from the caps at their boundary: with two whole workers
/// allowed, two requests hold a worker each and a third is stopped by the ceiling, and the
/// summary shows whole equal to the ceiling, every input of the ceiling and the reasons;
/// when one hold becomes a lease it counts as held, not as a mark, and whole stays at the
/// ceiling.
#[test]
fn the_summary_matches_the_caps_at_the_boundary() {
    let c = WholeNodeConfig {
        max_whole_node: 2,
        ..cfg(0, 2)
    };
    let (mut f, grants) = busy(c, &[("a", 600), ("b", 600), ("c", 600)]);
    let first = ask(&mut f);
    let _second = ask(&mut f);
    let third = ask(&mut f);
    assert_eq!(
        state(&f, third),
        WholeState::Stopped(WholeWait::Cap(Blocked::Ceiling))
    );
    let waiting = counts(&[
        (WholeReason::Holding, 2),
        (WholeReason::Cap(Blocked::Ceiling), 1),
    ]);
    assert_eq!(
        summary(&f),
        WholeSummary {
            held: 0,
            marks: 2,
            whole: 2,
            draining: 2,
            ceiling: 2,
            max_whole_node: 2,
            capable_hosts: 3,
            keep_for_ordinary: 0,
            max_draining: 9,
            waiting,
        }
    );
    let WholeState::Holding { worker, .. } = state(&f, first) else {
        panic!("holds");
    };
    let own = grants.iter().find(|g| g.worker == worker).unwrap();
    f.finish(own);
    assert_eq!(f.advance(f.now + 1_000).len(), 1);
    assert_eq!(f.s.whole_state(first), None);
    let s = summary(&f);
    assert_eq!((s.held, s.marks, s.whole, s.draining), (1, 1, 2, 1));
    assert_eq!(s.whole, s.ceiling);
    assert_eq!(
        s.waiting,
        counts(&[
            (WholeReason::Holding, 1),
            (WholeReason::Cap(Blocked::Ceiling), 1)
        ])
    );
}

/// Catches: the ceiling's inputs not read from the workers: a worker draining out of the
/// pool is not capable, so the capable hosts and with them the ceiling fall.
#[test]
fn the_summary_follows_the_capable_hosts() {
    let c = WholeNodeConfig {
        keep_for_ordinary: 1,
        ..cfg(0, 2)
    };
    let (mut f, _) = busy(c, &[("a", 600), ("b", 600), ("c", 600)]);
    let s = summary(&f);
    assert_eq!((s.capable_hosts, s.ceiling, s.whole), (3, 2, 0));
    assert_eq!(s.waiting, counts(&[]));
    f.feed(Event::Phase {
        worker: w("c"),
        phase: NodePhase::Draining,
    });
    let s = summary(&f);
    assert_eq!((s.capable_hosts, s.ceiling), (2, 1));
    assert_eq!((s.max_whole_node, s.max_draining), (9, 9));
}

/// Catches: the back-off read at the wrong time: after it ended and before any round the
/// request shows `Queued`, as it holds nothing and bars nothing now; a round then holds the
/// worker again.
#[test]
fn an_ended_backoff_shows_queued_until_the_next_round() {
    let (mut f, _) = busy(cfg(0, 2), &[("a", 600)]);
    let op = ask(&mut f);
    tick_at(&mut f, 70_000);
    assert!(matches!(state(&f, op), WholeState::Barred { .. }));
    f.at(85_000);
    f.beat();
    assert_eq!(state(&f, op), WholeState::Queued);
    tick_at(&mut f, 85_000);
    assert!(matches!(state(&f, op), WholeState::Holding { .. }));
}

/// Catches: `Barred` checked before `Stopped`: a request that gave up its hold on `a` (barred
/// there) and whose only other worker `b` is the last capable one of an older ordinary
/// request shows the stop, as `whole_wait` does.
#[test]
fn a_request_both_barred_and_stopped_shows_the_stop() {
    let mut f = Farm::new(
        Scheduler::new(1)
            .with_load_config(booked::booked())
            .with_whole_node(cfg(0, 2)),
    );
    let worker = |f: &mut Farm, name: &str, cores: u64| {
        f.feed(Event::WorkerUp {
            worker: w(name),
            instance: DaemonInstance::new(name),
            predecessor_stopped: false,
            capacity: Resources::new(cores * 1_000, 2 * cores * GIB),
            caps: caps(),
        });
        f.workers.push(w(name));
    };
    worker(&mut f, "a", 8);
    assert_eq!(f.lease(secs(60)).worker, w("a"));
    worker(&mut f, "b", 16);
    assert_eq!(f.lease(secs(600)).worker, w("b"));
    f.next_n += 1;
    f.submit(Request {
        resources: Resources::new(16_000, GIB),
        ..request(f.next_n, LeaseKind::Action)
    });
    assert!(f.grants().is_empty(), "only b could run it, and it is busy");
    let op = ask(&mut f);
    assert_eq!(state(&f, op), holding("a", 10_000, Some(70_000), 0));
    tick_at(&mut f, 70_000);
    assert_eq!(f.s.whole_give_ups(op), 1);
    assert_eq!(f.s.whole_wait(op), Some(WholeWait::LastCapable));
    assert_eq!(state(&f, op), WholeState::Stopped(WholeWait::LastCapable));
}

/// Catches: `Unservable` checked after `Early`: a request no worker could run is unservable
/// at once, though it is before `drain_after`.
#[test]
fn an_unservable_request_is_not_shown_as_early() {
    let (mut f, _) = busy(cfg(30_000, 2), &[("a", 600)]);
    f.next_n += 1;
    let op = f.s.next_operation();
    f.submit(Request {
        resources: Resources::new(64_000, GIB),
        ..request(f.next_n, LeaseKind::WholeMachine)
    });
    assert!(f.grants().is_empty());
    assert!(matches!(state(&f, op), WholeState::Unservable { .. }));
}
