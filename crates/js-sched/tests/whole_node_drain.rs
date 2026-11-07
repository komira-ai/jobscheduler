//! When a blocked whole-machine request holds a worker, and that it never lets go
//! ([`Scheduler::with_whole_node`]): not before `drain_after`, a quiet worker at once, a
//! deadline on each hold that is `drain_deadline` or the request's wall limit, and a hold
//! kept past that deadline until the worker empties, with no worker barred and
//! `max_reserve_attempts` read nowhere. Every time is farm time fed to the scheduler;
//! nothing waits.

#[macro_use]
#[path = "booked/mod.rs"]
mod booked;
#[allow(dead_code, reason = "each test binary uses some of the stand-in")]
#[path = "digest/mod.rs"]
mod digest;
#[allow(dead_code, reason = "each test binary uses some of the rig")]
#[path = "whole_rig/mod.rs"]
mod whole_rig;

use js_sched::{Bar, Event, Scheduler, WholeMark};
use js_types::sched::{FarmTime, OperationId};
use js_whole::WholeNodeConfig;

use crate::whole_rig::{Farm, secs, w};

const MAX_WALL: u64 = 600;

fn t(ms: u64) -> FarmTime {
    FarmTime::from_millis(ms)
}

/// Whole-node settings: a drain deadline of a minute, a back-off of ten seconds, and
/// `drain_after_ms` and the deprecated `attempts` as given.
fn cfg(drain_after_ms: u64, attempts: u32) -> WholeNodeConfig {
    WholeNodeConfig {
        drain_after_ms,
        drain_deadline_ms: Some(60_000),
        reserve_backoff_ms: 10_000,
        max_reserve_attempts: attempts,
        ..WholeNodeConfig::dedicated(4)
    }
}

/// One worker `a` with a lease that may run ten minutes, from farm time zero.
fn busy(config: Option<WholeNodeConfig>) -> Farm {
    let s = config.map_or_else(
        || Scheduler::new(1).with_load_config(booked::current()),
        |c| {
            Scheduler::new(1)
                .with_load_config(booked::current())
                .with_whole_node(c)
        },
    );
    let mut f = Farm::new(s);
    f.worker("a");
    f.lease(secs(MAX_WALL));
    f
}

/// Submits a whole-machine request with the ten-minute wall time at the current farm time
/// and ticks.
fn ask(f: &mut Farm) -> OperationId {
    let op = f.submit_whole(Some(secs(MAX_WALL)));
    assert!(f.grants().is_empty());
    op
}

fn tick_at(f: &mut Farm, ms: u64) {
    assert!(f.advance(ms).is_empty());
}

fn mark(f: &Farm, op: OperationId) -> Option<WholeMark> {
    f.s.whole_mark(op).cloned()
}

both_rules! {
/// Catches: `None` changed: a blocked whole-machine request holds a worker at once, with no
/// mark and no wait, as a scheduler without whole-node settings always did.
fn without_settings_a_blocked_request_holds_at_once_and_has_no_mark() {
    let mut f = busy(None);
    let op = ask(&mut f);
    assert_eq!(f.held_for(op), Some(&w("a")));
    assert_eq!(mark(&f, op), None);
    f.advance(10_000_000);
    assert_eq!(f.held_for(op), Some(&w("a")));
}
}

both_rules! {
/// Catches: the hold taken before `drain_after` has passed, or one tick late: nothing is
/// held a millisecond before it, the worker is held at it, and the mark carries the time.
fn a_busy_worker_is_held_only_after_drain_after() {
    let mut f = busy(Some(cfg(30_000, 2)));
    f.advance(1_000);
    let op = ask(&mut f);
    tick_at(&mut f, 30_999);
    assert_eq!(f.held_for(op), None);
    assert_eq!(mark(&f, op), None);
    tick_at(&mut f, 31_000);
    assert_eq!(f.held_for(op), Some(&w("a")));
    let m = mark(&f, op).expect("marked");
    assert_eq!((m.worker, m.made), (w("a"), t(31_000)));
}
}

both_rules! {
/// Catches: work that holds nothing before `drain_after` blocked behind the request: an
/// action submitted after it still has the worker's room.
fn before_drain_after_the_request_holds_back_no_one() {
    let mut f = busy(Some(cfg(30_000, 2)));
    let op = ask(&mut f);
    f.next_n += 1;
    f.submit(crate::whole_rig::request(
        f.next_n,
        js_types::sched::LeaseKind::Action,
    ));
    assert_eq!(f.grants().len(), 1);
    assert_eq!(f.held_for(op), None);
}
}

both_rules! {
/// Catches: `drain_after` applied to a quiet worker: one that fits is taken at once.
fn a_quiet_worker_is_taken_at_once_whatever_drain_after() {
    let mut f = busy(Some(cfg(30_000, 2)));
    f.worker("b");
    let op = f.submit_whole(Some(secs(MAX_WALL)));
    let grants = f.grants();
    assert_eq!(grants.len(), 1);
    assert_eq!((grants[0].operation, &grants[0].worker), (op, &w("b")));
    assert_eq!(mark(&f, op), None);
}
}

both_rules! {
/// Catches: the hold given up at its deadline: the mark is the same a millisecond before
/// the deadline, at it, and long after (a drain never ends while the worker's work runs),
/// and the request takes the worker when its lease ends.
fn a_whole_request_waits_past_the_old_deadline_without_giving_up() {
    let mut f = Farm::new(
        Scheduler::new(1)
            .with_load_config(booked::current())
            .with_whole_node(cfg(0, 2)),
    );
    f.worker("a");
    let lease = f.lease(secs(MAX_WALL));
    f.advance(5_000);
    let op = ask(&mut f);
    let first = mark(&f, op).expect("marked");
    assert_eq!((first.made, first.deadline), (t(5_000), t(65_000)));
    for at in [64_999, 65_000, 75_000, 500_000] {
        tick_at(&mut f, at);
        assert_eq!(mark(&f, op), Some(first.clone()), "at {at}");
        assert_eq!(f.held_for(op), Some(&w("a")), "at {at}");
        assert!(f.s.whole_barred(op, t(at)).is_empty(), "at {at}");
    }
    f.finish(&lease);
    let grants = f.advance(500_001);
    assert_eq!(grants.len(), 1, "{grants:?}");
    assert_eq!((grants[0].operation, &grants[0].worker), (op, &w("a")));
    assert_eq!(mark(&f, op), None);
}
}

both_rules! {
/// Catches: a hold past its deadline traded for another worker: `a` is the cheaper to
/// drain and is held; past the deadline, with `b` as busy as before, the mark stays on
/// `a` as it was made and `b` is never held.
fn past_its_deadline_a_mark_is_not_moved_to_another_busy_worker() {
    let mut f = busy(Some(cfg(0, 2)));
    f.worker("b");
    f.lease_on("b", secs(MAX_WALL * 2));
    let op = ask(&mut f);
    let m = mark(&f, op).expect("marked");
    assert_eq!((m.worker.clone(), m.deadline), (w("a"), t(60_000)));
    tick_at(&mut f, 60_000);
    tick_at(&mut f, 70_000);
    tick_at(&mut f, 300_000);
    assert_eq!(f.held_for(op), Some(&w("a")));
    assert_eq!(mark(&f, op), Some(m));
}
}

both_rules! {
/// Catches: a marked worker opened to later ordinary work once the deadline has passed: an
/// action with no estimate, submitted at the deadline, is not placed on the held worker
/// (it could only backfill, and nothing known ends in the tail), and the request still
/// holds it.
fn past_its_deadline_a_marked_worker_takes_no_later_ordinary_work() {
    let mut f = busy(Some(cfg(0, 2)));
    let op = ask(&mut f);
    tick_at(&mut f, 60_000);
    f.next_n += 1;
    f.submit(crate::whole_rig::request(
        f.next_n,
        js_types::sched::LeaseKind::Action,
    ));
    assert!(f.grants().is_empty());
    assert_eq!(f.held_for(op), Some(&w("a")));
}
}

both_rules! {
/// Catches: `max_reserve_attempts` still read: with one attempt or five the mark, its
/// deadline (the configured one, not the wall limit) and the worker held are the same,
/// before and long after the deadline.
fn deprecated_max_reserve_attempts_does_nothing() {
    let run = |attempts| {
        let mut f = busy(Some(cfg(0, attempts)));
        let op = ask(&mut f);
        let made = mark(&f, op);
        tick_at(&mut f, 60_000);
        tick_at(&mut f, 400_000);
        (made, mark(&f, op), f.held_for(op).cloned())
    };
    let one = run(1);
    assert_eq!(one.0.as_ref().map(|m| m.deadline), Some(t(60_000)));
    assert_eq!(one.1, one.0);
    assert_eq!(one.2, Some(w("a")));
    assert_eq!(run(5), one);
}
}

both_rules! {
/// Catches: an unset drain deadline taken as zero or as a default of its own: the
/// deadline is the request's wall limit after the mark, and the hold is kept past it.
fn with_no_drain_deadline_the_deadline_is_the_wall_limit() {
    let mut c = cfg(0, 2);
    c.drain_deadline_ms = None;
    let mut f = busy(Some(c));
    f.advance(2_000);
    let op = ask(&mut f);
    let m = mark(&f, op).expect("marked");
    assert_eq!(m.deadline, t(2_000 + MAX_WALL * 1_000));
    tick_at(&mut f, 2_000 + MAX_WALL * 3_000);
    assert_eq!(mark(&f, op), Some(m));
}
}

both_rules! {
/// Catches: a request with no max wall time given a short deadline: with no drain
/// deadline set either, its deadline is the end of time.
fn a_request_with_no_wall_time_and_no_drain_deadline_has_no_end() {
    let mut c = cfg(0, 2);
    c.drain_deadline_ms = None;
    let mut f = busy(Some(c));
    let op = f.submit_whole(None);
    assert!(f.grants().is_empty());
    assert_eq!(mark(&f, op).map(|m| m.deadline), Some(t(u64::MAX)));
    tick_at(&mut f, 10_000_000);
    assert_eq!(f.held_for(op), Some(&w("a")));
}
}

both_rules! {
/// Catches: the mark outliving the placement: when the worker empties the request takes
/// it and the mark is gone.
fn placement_clears_the_mark() {
    let mut f = Farm::new(
        Scheduler::new(1)
            .with_load_config(booked::current())
            .with_whole_node(cfg(0, 2)),
    );
    f.worker("a");
    let lease = f.lease(secs(MAX_WALL));
    let op = ask(&mut f);
    assert!(mark(&f, op).is_some());
    f.advance(20_000);
    f.finish(&lease);
    let grants = f.grants();
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0].operation, op);
    assert_eq!(mark(&f, op), None);
    assert_eq!(f.held_for(op), None);
}
}

both_rules! {
/// Catches: a cancelled request's mark left behind, past its deadline too: the mark goes
/// with the request, and the next request marks the worker afresh.
fn a_cancelled_request_has_no_mark() {
    let mut f = busy(Some(cfg(0, 2)));
    let op = ask(&mut f);
    tick_at(&mut f, 60_000);
    assert!(mark(&f, op).is_some());
    f.s.forget_queued(op).expect("queued");
    assert_eq!(mark(&f, op), None);
    let op2 = ask(&mut f);
    assert_ne!(op, op2);
    let m = mark(&f, op2).expect("marked");
    assert_eq!((m.worker, m.made), (w("a"), t(60_000)));
    f.s.forget_queued(op2).expect("queued");
    assert_eq!(mark(&f, op2), None);
}
}

both_rules! {
/// Catches: a worker that cannot be held any more leaving its mark: the mark moves to the
/// other worker with the deadline from the move and the time it was first made, and the
/// worker it left is not barred.
fn a_worker_that_goes_away_moves_the_mark() {
    let mut f = busy(Some(cfg(0, 2)));
    f.worker("b");
    f.lease_on("b", secs(MAX_WALL * 2));
    let op = ask(&mut f);
    assert_eq!(f.held_for(op), Some(&w("a")));
    f.advance(20_000);
    f.feed(Event::Cordon { worker: w("a") });
    f.grants();
    assert_eq!(f.held_for(op), Some(&w("b")));
    // A mark pushed off a worker that can no longer hold its request keeps the time it
    // was made (0), not the time of the move; the deadline follows the move.
    assert_eq!(
        mark(&f, op).map(|m| (m.made, m.deadline)),
        Some((t(0), t(80_000)))
    );
    assert!(f.s.whole_barred(op, t(20_000)).is_empty());
}
}

both_rules! {
/// Catches: a deadline that stops or gives up running work: the round at the deadline
/// proposes nothing, the lease on the held worker is still held and running, and it
/// finishes as a result as before.
fn the_deadline_stops_nothing() {
    let mut f = Farm::new(
        Scheduler::new(1)
            .with_load_config(booked::current())
            .with_whole_node(cfg(0, 2)),
    );
    f.worker("a");
    let lease = f.lease(secs(MAX_WALL));
    let op = ask(&mut f);
    f.advance(59_999);
    f.at(60_000);
    f.beat();
    let effects = f.feed(Event::Tick);
    assert!(effects.is_empty(), "{effects:?}");
    assert!(mark(&f, op).is_some());
    assert_eq!(f.s.leases_held(), 1);
    f.finish(&lease);
    assert_eq!(f.s.leases_held(), 0);
}
}

both_rules! {
/// Catches: a worker in a back-off taken because it emptied while another worker could
/// still run the request: `a` is backed off until 70 s with `bar_whole`, the request holds
/// `b`, `a`'s lease ends, and the request takes `a` only from the moment the back-off ends.
fn a_backed_off_worker_that_empties_is_taken_only_after_the_backoff() {
    let mut f = Farm::new(
        Scheduler::new(1)
            .with_load_config(booked::current())
            .with_whole_node(cfg(0, 2)),
    );
    f.worker("a");
    let short = f.lease(secs(MAX_WALL));
    f.worker("b");
    f.lease_on("b", secs(MAX_WALL * 2));
    let op = ask(&mut f);
    assert_eq!(f.held_for(op), Some(&w("a")));
    assert_eq!(f.s.bar_whole(op, w("a"), Bar::Backoff(t(70_000))), Ok(()));
    tick_at(&mut f, 1_000);
    assert_eq!(f.held_for(op), Some(&w("b")));
    f.finish(&short);
    tick_at(&mut f, 69_999);
    let grants = f.advance(70_000);
    assert_eq!(grants.len(), 1);
    assert_eq!((grants[0].operation, &grants[0].worker), (op, &w("a")));
}
}

both_rules! {
/// Catches: a back-off that keeps a request off its only worker when that worker empties:
/// with `a` backed off until 70 s and no other worker, the request holds nothing, and when
/// `a`'s lease ends it is granted `a` at once, inside the back-off.
fn the_only_worker_is_taken_when_it_empties_during_a_backoff() {
    let mut f = Farm::new(
        Scheduler::new(1)
            .with_load_config(booked::current())
            .with_whole_node(cfg(0, 2)),
    );
    f.worker("a");
    let lease = f.lease(secs(MAX_WALL));
    let op = ask(&mut f);
    assert_eq!(f.s.bar_whole(op, w("a"), Bar::Backoff(t(70_000))), Ok(()));
    tick_at(&mut f, 1_000);
    assert_eq!(f.held_for(op), None);
    f.finish(&lease);
    let grants = f.advance(65_000);
    assert_eq!(grants.len(), 1, "{grants:?}");
    assert_eq!((grants[0].operation, &grants[0].worker), (op, &w("a")));
    assert_eq!(mark(&f, op), None);
}
}
