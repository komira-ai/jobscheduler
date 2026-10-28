//! When a blocked whole-machine request holds a worker and when it lets go
//! ([`Scheduler::with_whole_node`]): not before `drain_after`, a quiet worker at once, a
//! deadline on each hold, a give-up that bars the worker for `reserve_backoff`, and the
//! request's max wall time as the deadline once it has given up `max_reserve_attempts`
//! times. Every time is farm time fed to the scheduler; nothing waits.

#[allow(dead_code, reason = "each test binary uses some of the stand-in")]
#[path = "digest/mod.rs"]
mod digest;
#[allow(dead_code, reason = "each test binary uses some of the rig")]
#[path = "whole_rig/mod.rs"]
mod whole_rig;

use js_sched::{Event, Scheduler, WholeMark};
use js_types::sched::{FarmTime, OperationId};
use js_whole::WholeNodeConfig;

use crate::whole_rig::{Farm, secs, w};

const MAX_WALL: u64 = 600;

fn t(ms: u64) -> FarmTime {
    FarmTime::from_millis(ms)
}

/// Whole-node settings: a drain deadline of a minute, a back-off of ten seconds, and
/// `drain_after_ms` and `attempts` as given.
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
        || Scheduler::new(1),
        |c| Scheduler::new(1).with_whole_node(c),
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

/// Catches: `None` changed: a blocked whole-machine request holds a worker at once, with no
/// mark and no wait, as a scheduler without whole-node settings always did.
#[test]
fn without_settings_a_blocked_request_holds_at_once_and_has_no_mark() {
    let mut f = busy(None);
    let op = ask(&mut f);
    assert_eq!(f.held_for(op), Some(&w("a")));
    assert_eq!(mark(&f, op), None);
    f.advance(10_000_000);
    assert_eq!(f.held_for(op), Some(&w("a")));
    assert_eq!(f.s.whole_give_ups(op), 0);
}

/// Catches: the hold taken before `drain_after` has passed, or one tick late: nothing is
/// held a millisecond before it, the worker is held at it, and the mark carries the time.
#[test]
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

/// Catches: work that holds nothing before `drain_after` blocked behind the request: an
/// action submitted after it still has the worker's room.
#[test]
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

/// Catches: `drain_after` applied to a quiet worker: one that fits is taken at once.
#[test]
fn a_quiet_worker_is_taken_at_once_whatever_drain_after() {
    let mut f = busy(Some(cfg(30_000, 2)));
    f.worker("b");
    let op = f.submit_whole(Some(secs(MAX_WALL)));
    let grants = f.grants();
    assert_eq!(grants.len(), 1);
    assert_eq!((grants[0].operation, &grants[0].worker), (op, &w("b")));
    assert_eq!(mark(&f, op), None);
}

/// Catches: the deadline counted from the wrong time or off by one: the hold is kept a
/// millisecond before it and given up at it, with one give-up counted, no mark, and, on
/// the only worker there is, nothing held while its back-off lasts.
#[test]
fn the_hold_is_given_up_at_its_deadline() {
    let mut f = busy(Some(cfg(0, 2)));
    f.advance(5_000);
    let op = ask(&mut f);
    let first = mark(&f, op).expect("marked");
    assert_eq!((first.made, first.deadline), (t(5_000), t(65_000)));
    tick_at(&mut f, 64_999);
    assert_eq!(mark(&f, op), Some(first));
    assert_eq!(f.s.whole_give_ups(op), 0);
    tick_at(&mut f, 65_000);
    assert_eq!(f.s.whole_give_ups(op), 1);
    assert_eq!(f.held_for(op), None);
    assert_eq!(mark(&f, op), None);
}

/// Catches: the back-off ending early or late, or not binding at all: the worker is barred
/// until exactly `reserve_backoff` after the give-up, and is held again from then, with a
/// new mark.
#[test]
fn the_given_up_worker_is_barred_until_exactly_the_backoff_ends() {
    let mut f = busy(Some(cfg(0, 2)));
    let op = ask(&mut f);
    tick_at(&mut f, 60_000);
    tick_at(&mut f, 69_999);
    assert_eq!(f.held_for(op), None);
    tick_at(&mut f, 70_000);
    assert_eq!(f.held_for(op), Some(&w("a")));
    let again = mark(&f, op).expect("marked again");
    assert_eq!((again.made, again.deadline), (t(70_000), t(130_000)));
    assert_eq!(f.s.whole_give_ups(op), 1);
}

/// Catches: no other worker tried after a give-up: `a` is the cheaper to drain and is held
/// first; given up, the request holds `b` with a deadline from the give-up, and keeps it
/// when `a`'s back-off ends.
#[test]
fn a_second_worker_is_held_after_a_give_up() {
    let mut f = busy(Some(cfg(0, 2)));
    f.worker("b");
    assert_eq!(f.lease(secs(MAX_WALL * 2)).worker, w("b"));
    let op = ask(&mut f);
    assert_eq!(f.held_for(op), Some(&w("a")));
    tick_at(&mut f, 60_000);
    assert_eq!(f.held_for(op), Some(&w("b")));
    let m = mark(&f, op).expect("marked");
    assert_eq!(
        (m.worker, m.made, m.deadline),
        (w("b"), t(60_000), t(120_000))
    );
    tick_at(&mut f, 70_000);
    assert_eq!(f.held_for(op), Some(&w("b")));
    assert_eq!(mark(&f, op).map(|m| m.made), Some(t(60_000)));
}

/// Catches: the attempt cap ignored: with one attempt allowed, after one give-up the next
/// hold's deadline is the max wall time and it is not given up at the shorter drain
/// deadline; the worker is taken when its lease ends.
#[test]
fn after_the_attempt_cap_the_deadline_is_the_max_wall_time() {
    let mut f = busy(Some(cfg(0, 1)));
    let op = ask(&mut f);
    tick_at(&mut f, 60_000);
    tick_at(&mut f, 70_000);
    let m = mark(&f, op).expect("held again");
    assert_eq!(m.deadline, t(70_000 + MAX_WALL * 1_000));
    tick_at(&mut f, 70_000 + 60_000);
    tick_at(&mut f, 70_000 + 3 * 60_000);
    assert_eq!(f.s.whole_give_ups(op), 1);
    assert_eq!(f.held_for(op), Some(&w("a")));
}

/// Catches: a request with no max wall time given up as if it had a short one, or given
/// none at all: with no drain deadline set either, it is never given up.
#[test]
fn a_request_with_no_wall_time_and_no_drain_deadline_is_never_given_up() {
    let mut c = cfg(0, 2);
    c.drain_deadline_ms = None;
    let mut f = busy(Some(c));
    let op = f.submit_whole(None);
    assert!(f.grants().is_empty());
    assert_eq!(mark(&f, op).map(|m| m.deadline), Some(t(u64::MAX)));
    tick_at(&mut f, 10_000_000);
    assert_eq!(f.s.whole_give_ups(op), 0);
    assert_eq!(f.held_for(op), Some(&w("a")));
}

/// Catches: the mark outliving the placement: when the worker empties the request takes
/// it and the mark is gone.
#[test]
fn placement_clears_the_mark() {
    let mut f = Farm::new(Scheduler::new(1).with_whole_node(cfg(0, 2)));
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

/// Catches: a cancelled request's mark and counters left behind.
#[test]
fn a_cancelled_request_has_no_mark_and_no_count() {
    let mut f = busy(Some(cfg(0, 2)));
    let op = ask(&mut f);
    tick_at(&mut f, 60_000);
    assert_eq!(f.s.whole_give_ups(op), 1);
    f.s.forget_queued(op).expect("queued");
    assert_eq!(f.s.whole_give_ups(op), 0);
    let op2 = ask(&mut f);
    assert_ne!(op, op2);
    assert!(mark(&f, op2).is_some());
    f.s.forget_queued(op2).expect("queued");
    assert_eq!(mark(&f, op2), None);
}

/// Catches: a worker that cannot be held any more leaving its mark and costing an attempt:
/// the mark moves to the other worker with the deadline from the move, and no give-up is
/// counted.
#[test]
fn a_worker_that_goes_away_moves_the_mark_without_a_give_up() {
    let mut f = busy(Some(cfg(0, 2)));
    f.worker("b");
    assert_eq!(f.lease(secs(MAX_WALL * 2)).worker, w("b"));
    let op = ask(&mut f);
    assert_eq!(f.held_for(op), Some(&w("a")));
    f.advance(20_000);
    f.feed(Event::Cordon { worker: w("a") });
    f.grants();
    assert_eq!(f.held_for(op), Some(&w("b")));
    assert_eq!(
        mark(&f, op).map(|m| (m.made, m.deadline)),
        Some((t(20_000), t(80_000)))
    );
    assert_eq!(f.s.whole_give_ups(op), 0);
}

/// Catches: a give-up that stops or gives up running work: the round at the deadline
/// proposes nothing, the lease on the held worker is still held and running, and it
/// finishes as a result as before.
#[test]
fn a_give_up_stops_nothing() {
    let mut f = Farm::new(Scheduler::new(1).with_whole_node(cfg(0, 2)));
    f.worker("a");
    let lease = f.lease(secs(MAX_WALL));
    let op = ask(&mut f);
    f.advance(59_999);
    f.at(60_000);
    f.beat();
    let effects = f.feed(Event::Tick);
    assert!(effects.is_empty(), "{effects:?}");
    assert_eq!(f.s.whole_give_ups(op), 1);
    assert_eq!(f.s.leases_held(), 1);
    f.finish(&lease);
    assert_eq!(f.s.leases_held(), 0);
}

/// Catches: a worker taken in its back-off because it emptied: `a` is given up and then
/// its lease ends while another worker is still held, and the request takes it only from
/// the moment the back-off ends.
#[test]
fn a_given_up_worker_that_empties_is_taken_only_after_the_backoff() {
    let mut f = Farm::new(Scheduler::new(1).with_whole_node(cfg(0, 2)));
    f.worker("a");
    let short = f.lease(secs(MAX_WALL));
    f.worker("b");
    assert_eq!(f.lease(secs(MAX_WALL * 2)).worker, w("b"));
    let op = ask(&mut f);
    tick_at(&mut f, 60_000);
    assert_eq!(f.held_for(op), Some(&w("b")));
    f.finish(&short);
    tick_at(&mut f, 69_999);
    let grants = f.advance(70_000);
    assert_eq!(grants.len(), 1);
    assert_eq!((grants[0].operation, &grants[0].worker), (op, &w("a")));
}
