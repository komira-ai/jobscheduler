//! The scheduler's rules for what it lists as unfinished and how long it keeps a finished
//! operation.
//!
//! A module of the `scheduler` test binary, for its harness.

use std::time::Duration;

use js_sched::{Event, FINISHED_RETENTION, OpState, Scheduler};
use js_types::sched::{
    ControlRecord, Effect, LeaseGrant, LeaseId, OperationId, Resources, ResultRecord, WaiterId,
};

use super::{GIB, Harness, booked, ok, request, w};

/// Runs one operation on `a` from submit to answer at the harness's time, with a
/// heartbeat first so `a` stays live. Returns its grant.
fn run_one(h: &mut Harness, waiter: u64, n: u8) -> LeaseGrant {
    h.heartbeat("a");
    h.submit(waiter, request(n));
    let [grant] = h.tick().try_into().unwrap();
    h.commit_and_start(&grant);
    h.finish(&grant, ok(n));
    grant
}

both_rules! {
/// Catches: a list of unfinished operations that includes a finished one (a successor
/// process would follow results already given) or leaves out a queued or running one
/// (a caller left with no answer).
///
/// Three operations: one finished, one running, one queued behind it on a node with
/// room for one.
fn unfinished_lists_the_operations_not_done_and_no_other() {
    let mut h = Harness::new();
    h.worker("a", 1_000, GIB);
    let done = run_one(&mut h, 0, 0);
    h.submit(1, request(1));
    let [running] = h.tick().try_into().unwrap();
    h.commit_and_start(&running);
    h.submit(2, request(2));
    assert!(h.s.state(done.operation).is_some_and(OpState::is_done));
    assert_eq!(
        h.s.unfinished().collect::<Vec<_>>(),
        [running.operation, OperationId(2)]
    );
}
}

both_rules! {
/// Catches: finished operations kept forever, so the scheduler's memory
/// grows with every operation it ever ran; one dropped before the retention is up, so
/// a WaitExecution right after a broken stream finds nothing; and a drop that takes
/// only one expired operation per input, which falls behind a burst.
///
/// One operation finishes each second for 100 s. The operations held are exactly
/// those finished in the last retention (60 s), never more; one input at the end of
/// the retention drops all the rest at once.
fn a_finished_operation_is_dropped_once_the_retention_is_up() {
    let retention = FINISHED_RETENTION.as_secs();
    let mut h = Harness::new();
    h.worker("a", 1_000, GIB);
    let mut grants = Vec::new();
    for i in 0..100u64 {
        h.at_secs(i);
        grants.push(run_one(&mut h, i, u8::try_from(i).unwrap()));
        let kept = (i + 1).min(retention);
        assert_eq!(u64::try_from(h.s.operations()), Ok(kept), "at {i} s");
        // The newest finished operations are still there, the older ones gone.
        let oldest_kept = i + 1 - kept;
        for (j, grant) in (0u64..).zip(&grants) {
            let held = h.s.state(grant.operation).is_some();
            assert_eq!(held, j >= oldest_kept, "operation {j} at {i} s");
        }
    }
    let last = grants.last().unwrap();

    // Within the retention the last one is still there, finished, with its waiter.
    h.at_millis((99 + retention) * 1_000 - 1).heartbeat("a");
    assert!(h.s.state(last.operation).is_some_and(OpState::is_done));
    assert_eq!(h.s.waiters(last.operation), Some(&[WaiterId(99)][..]));

    // One input at the end of the retention drops every one left.
    h.at_secs(99 + retention).heartbeat("a");
    assert_eq!(h.s.operations(), 0);
    assert_eq!(h.s.state(last.operation), None);
    assert_eq!(h.s.waiters(last.operation), None);
}
}

both_rules! {
/// Catches: a dropped operation that a late input brings back or acts on. A stale
/// result or grant commit, and a late report or start for it, are dropped as for an
/// unknown operation; its lease, still listed by the worker, is named not held (so
/// the worker is told to cancel it); and the same action again is a new operation.
fn late_inputs_for_a_dropped_operation_are_dropped() {
    let retention = Duration::from_secs(5);
    let mut h = Harness::with(
        Scheduler::new(1)
            .with_load_config(booked::current())
            .with_finished_retention(retention),
    );
    h.worker("a", 1_000, GIB);
    let grant = run_one(&mut h, 1, 1);
    let record = ControlRecord::Result(ResultRecord {
        lease: grant.lease,
        operation: grant.operation,
        outcome: ok(1),
    });
    h.at_secs(5).heartbeat("a");
    assert_eq!(h.s.state(grant.operation), None);

    assert!(h.commit(record).is_empty(), "a dropped operation answered");
    assert!(h.commit(ControlRecord::Lease(grant.clone())).is_empty());
    assert!(h.report(&grant, ok(1)).is_empty());
    let started = Event::Started {
        operation: grant.operation,
        lease: grant.lease,
    };
    assert!(h.feed(started).is_empty());
    assert_eq!(h.s.state(grant.operation), None, "brought back");
    let listed = [grant.lease];
    let not_held: Vec<LeaseId> = h.s.not_held(&w("a"), &listed).collect();
    assert_eq!(not_held, listed);

    h.submit(2, request(1));
    let [again] = h.tick().try_into().unwrap();
    assert_ne!(again.operation, grant.operation);
    assert_eq!(h.s.operations(), 1);
}
}

both_rules! {
/// Catches: a retention other than the configured one, and a refused operation that
/// is never dropped (only answered ones are). A zero retention drops an answered
/// operation in the input that answers it; the answer still names its waiters.
fn the_retention_is_configurable_and_holds_for_refusals_too() {
    let mut h = Harness::with(
        Scheduler::new(1)
            .with_load_config(booked::current())
            .with_finished_retention(Duration::ZERO),
    );
    h.worker("a", 1_000, GIB);
    h.submit(1, request(1));
    let [grant] = h.tick().try_into().unwrap();
    h.commit_and_start(&grant);
    let answer = h.finish(&grant, ok(1));
    assert_eq!(answer.waiters, [WaiterId(1)]);
    assert_eq!(h.s.operations(), 0, "kept with a zero retention");

    let s = Scheduler::new(1)
        .with_load_config(booked::current())
        .with_unservable_wait(Duration::from_secs(1))
        .with_finished_retention(Duration::from_secs(10));
    let mut h = Harness::with(s);
    h.worker("a", 1_000, GIB);
    let mut big = request(1);
    big.resources = Resources::new(2_000, GIB);
    h.submit(1, big);
    h.feed(Event::Tick);
    let record = h
        .at_secs(1)
        .feed(Event::Tick)
        .into_iter()
        .find_map(|e| match e {
            Effect::Commit(record @ ControlRecord::Refusal(_)) => Some(record),
            _ => None,
        })
        .expect("a refusal proposed");
    h.commit(record);
    let id = OperationId(0);
    assert!(matches!(h.s.state(id), Some(OpState::Refused { .. })));
    h.at_millis(10_999).heartbeat("a");
    assert!(h.s.state(id).is_some(), "dropped within the retention");
    h.at_secs(11).heartbeat("a");
    assert_eq!(h.s.state(id), None, "a refused operation kept");
    assert_eq!(h.s.operations(), 0);
}
}
