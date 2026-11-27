//! The two settings of [`Backfill`] that time leases ([`Scheduler::with_backfill`]): the
//! margin ([`Backfill::with_margin`]), which moves both the backfill limit and the time a
//! lease past its end is read to end at, and the ceiling of enforced bounds, at which a lease
//! with no end known counts when a worker is picked to drain. Every time is farm time fed to
//! the scheduler.

#[macro_use]
#[path = "booked/mod.rs"]
mod booked;
#[allow(dead_code, reason = "each test binary uses some of the stand-in")]
#[path = "digest/mod.rs"]
mod digest;

#[allow(dead_code, reason = "each test binary uses some of the rig")]
#[path = "whole_rig/mod.rs"]
mod whole_rig;

use std::time::Duration;

use crate::whole_rig::{Farm, request, secs, w};
use js_sched::{Backfill, Scheduler};
use js_types::sched::{FarmTime, LeaseKind, OperationId, WorkerId};
use js_whole::WholeNodeConfig;

/// A ceiling no wall time of the margin tests reaches.
const DAY: Duration = Duration::from_secs(86_400);

/// A scheduler with whole-node settings (four marks may drain at once), placing by the
/// booked rule, with `backfill`.
fn farm(backfill: Option<Backfill>) -> Farm {
    let whole = WholeNodeConfig {
        max_draining: 4,
        ..WholeNodeConfig::dedicated(4)
    };
    let s = Scheduler::new(1)
        .with_load_config(booked::booked())
        .with_whole_node(whole);
    Farm::new(match backfill {
        Some(backfill) => s.with_backfill(backfill),
        None => s,
    })
}

/// Bounds enforced up to `ceiling`, with a margin of two minutes.
fn two_minutes(ceiling: Duration) -> Option<Backfill> {
    Some(Backfill::enforced(ceiling).with_margin(secs(120)))
}

/// Submits an action that may run `wall` at the farm time now and runs a round: the worker
/// it was placed on, if it was.
fn job(f: &mut Farm, wall: Duration) -> Option<WorkerId> {
    f.next_n += 1;
    let operation = f.s.next_operation();
    f.s.set_max_wall(operation, wall);
    f.submit(request(f.next_n, LeaseKind::Action));
    let grant = f.grants().into_iter().find(|g| g.operation == operation)?;
    f.start(&grant);
    Some(grant.worker)
}

/// The tail end of `operation`'s mark, in farm milliseconds.
fn tail_end(f: &Farm, operation: OperationId) -> Option<u64> {
    let mark =
        f.s.whole_mark(operation)
            .expect("the request marks a worker");
    mark.tail_end.map(FarmTime::as_millis)
}

/// Catches: a margin set and not kept by the backfill check (the minute kept instead).
/// Worker `a`'s lease ends at 600 s and a whole-machine request holds it; at 10 s the tail
/// leaves 590 s, of which two minutes are kept spare, so a job of 470 s fits exactly and
/// one of 470.001 s does not, nor does one of 530 s, which a minute's margin would let in.
#[test]
fn a_margin_set_moves_the_backfill_limit() {
    let mut f = farm(two_minutes(DAY));
    f.worker("a");
    f.lease_on("a", secs(600));
    let whole = f.whole();
    assert_eq!(f.held_for(whole), Some(&w("a")));
    f.at(10_000);
    f.beat();
    assert_eq!(job(&mut f, secs(530)), None);
    assert_eq!(job(&mut f, Duration::from_millis(470_001)), None);
    assert_eq!(job(&mut f, secs(470)), Some(w("a")));
}

/// `a` holds a lease of 60 s, past its bound at 100 s; `b` then takes a lease of `b_wall`
/// seconds, and a whole-machine request arrives: the farm and the request.
fn late_a_against_b(backfill: Option<Backfill>, b_wall: u64) -> (Farm, OperationId) {
    let mut f = farm(backfill);
    f.worker("a");
    f.lease_on("a", secs(60));
    f.worker("b");
    assert!(f.advance(100_000).is_empty());
    f.lease_on("b", secs(b_wall));
    let whole = f.whole();
    (f, whole)
}

/// Catches: a margin set and not kept by the reading of a late lease (the minute kept
/// instead). At 100 s `a`'s lease of 60 s still runs and reads as ending two minutes from
/// now, at 220 s: against a lease of `b` ending 119 s from now `b` is held; against one
/// ending 120 s from now the two cost the same and the lower name `a` is held, with the
/// tail of 220 s. With a minute's margin `a` would read as ending at 160 s and be held
/// against both.
#[test]
fn a_margin_set_moves_the_end_a_late_lease_is_read_at() {
    let (f, whole) = late_a_against_b(two_minutes(DAY), 119);
    assert_eq!(f.held_for(whole), Some(&w("b")));
    let (f, whole) = late_a_against_b(two_minutes(DAY), 120);
    assert_eq!(f.held_for(whole), Some(&w("a")));
    assert_eq!(tail_end(&f, whole), Some(220_000));
}

/// Worker `a` with a lease of no wall time and no estimate from 0, worker `b` with one of
/// `b_wall` from 0, and a whole-machine request: the farm and the request.
fn unknown_a_against_b(backfill: Option<Backfill>, b_wall: Duration) -> (Farm, OperationId) {
    let mut f = farm(backfill);
    f.worker("a");
    f.place_on("a", None);
    f.worker("b");
    f.lease_on("b", b_wall);
    let whole = f.whole();
    (f, whole)
}

/// Catches: a lease with no end known ranked after every known one though bounds are
/// enforced. With a ceiling of a day, `a`'s lease ends by a day after it started, before
/// `b`'s of two days: `a` is held. Its mark keeps no tail end, so nothing backfills there.
#[test]
fn with_bounds_enforced_an_unknown_end_counts_as_the_ceiling() {
    let (f, whole) = unknown_a_against_b(Some(Backfill::enforced(DAY)), 2 * DAY);
    assert_eq!(f.held_for(whole), Some(&w("a")));
    assert_eq!(tail_end(&f, whole), None);
}

/// Catches: the unknown end counted at other than its start plus the ceiling. With a
/// ceiling of 600 s, `a`'s lease counts as ending at 600 s: against `b`'s bound of 599 s
/// `b` is held; against 600 s the two cost the same and the lower name `a` is held.
#[test]
fn an_unknown_end_counts_as_its_start_plus_the_ceiling_exactly() {
    let ceiling = Some(Backfill::enforced(secs(600)));
    let (f, whole) = unknown_a_against_b(ceiling, secs(599));
    assert_eq!(f.held_for(whole), Some(&w("b")));
    let (f, whole) = unknown_a_against_b(ceiling, secs(600));
    assert_eq!(f.held_for(whole), Some(&w("a")));
    assert_eq!(tail_end(&f, whole), None);
}

/// Catches: an unknown end counted at the ceiling while bounds are not enforced. Without
/// them `a`'s lease has no end, and `a` ranks after `b`, whose lease ends in two days.
#[test]
fn without_bounds_enforced_an_unknown_end_ranks_last() {
    let (f, whole) = unknown_a_against_b(None, 2 * DAY);
    assert_eq!(f.held_for(whole), Some(&w("b")));
    let off = Some(Backfill::default().with_margin(secs(120)));
    let (f, whole) = unknown_a_against_b(off, 2 * DAY);
    assert_eq!(f.held_for(whole), Some(&w("b")));
}

/// Catches: an unknown end counted at the ceiling read as ending then even once that has
/// passed, or floored by other than the margin set. With a ceiling of 60 s and a margin of
/// two minutes, at 100 s `a`'s lease (from 0) is past its start plus the ceiling and reads
/// as ending at 220 s, after `b`'s lease of 90 s from 100 s (190 s): `b` is held. Read as
/// ending at 60 s, or a minute from now (160 s), `a` would be the cheaper and be held.
#[test]
fn an_unknown_end_past_the_ceiling_reads_as_ending_the_margin_from_now() {
    let mut f = farm(two_minutes(secs(60)));
    f.worker("a");
    f.place_on("a", None);
    f.worker("b");
    assert!(f.advance(100_000).is_empty());
    f.lease_on("b", secs(90));
    let whole = f.whole();
    assert_eq!(f.held_for(whole), Some(&w("b")));
}

/// Catches: a lease placed in the round of the pick, with no end known, left out of the
/// order's reading of the ceiling, or its worker's tail kept as known. `a` runs a lease of
/// two days and `b` one of three; an action with no wall time, held to `a`, and the
/// whole-machine request come in one round, and the action is placed on `a` first. With a
/// ceiling of a day it counts as ending a day from now, inside `a`'s two days, so `a` is
/// still the cheaper and is held, with no tail end: its new lease's end is not known.
#[test]
fn a_lease_placed_in_the_round_with_no_end_known_counts_as_the_ceiling() {
    let mut f = farm(Some(Backfill::enforced(DAY)));
    f.worker("a");
    f.lease_on("a", 2 * DAY);
    f.worker("b");
    f.lease_on("b", 3 * DAY);
    f.next_n += 1;
    let action = f.s.next_operation();
    f.submit(request(f.next_n, LeaseKind::Action));
    f.s.place_only_on(action, [w("a")].into());
    let whole = f.submit_whole(None);
    let grants = f.grants();
    let placed = grants.iter().find(|g| g.operation == action);
    assert_eq!(placed.map(|g| &g.worker), Some(&w("a")));
    assert_eq!(f.held_for(whole), Some(&w("a")));
    assert_eq!(tail_end(&f, whole), None);
}

/// Catches: a mark on a worker whose end is counted at the ceiling moved on a tie, because
/// its tail is unknown for backfill while the other's is known. At 0 `a` runs leases of
/// 900 s and 600 s and `b` one with no end known (600 s with a ceiling of 600 s), so `b`
/// is marked. At 100 s `a`'s 900 s lease ends and its tail is 600 s, the same as `b`'s:
/// the mark stays on `b`.
#[test]
fn a_mark_counted_at_the_ceiling_stays_on_a_tie() {
    let mut f = farm(Some(Backfill::enforced(secs(600))));
    f.worker("a");
    let long = f.lease_on("a", secs(900));
    f.lease_on("a", secs(600));
    f.worker("b");
    f.place_on("b", None);
    let whole = f.whole();
    assert_eq!(f.held_for(whole), Some(&w("b")));
    f.at(100_000);
    f.finish(&long);
    f.beat();
    assert!(f.grants().is_empty());
    assert_eq!(f.held_for(whole), Some(&w("b")));
}
