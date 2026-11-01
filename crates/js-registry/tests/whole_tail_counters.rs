//! The evaluation of the tail rule: the end of the tail a mark was made with, kept in the
//! reservation ledger; the leases started on marked hosts and the share that was
//! backfill; and the error of the end estimated for each lease that ended. The registry
//! is driven on a manual clock; nothing sleeps and no test asserts how long anything took.

mod support;

use std::collections::BTreeMap;
use std::sync::Arc;

use js_provider::Provider;
use js_registry::{ERROR_EDGES_MS, ProfileLives, TailCounters};
use js_usage::ProfileKey;
use support::world::*;
use support::*;

/// A fixed table of P90 minutes by profile name.
struct Table(BTreeMap<&'static str, u32>);

impl ProfileLives for Table {
    fn wall_minutes_p90(&self, key: &ProfileKey) -> Option<u32> {
        self.0.get(key.as_str()).copied()
    }
}

/// One host `a` and the table of lives set.
fn world() -> World {
    let w = World::new(&["a"], plain);
    let table = Table(BTreeMap::from([("fast", 1), ("two", 2)]));
    w.fx.registry.set_lives(Arc::new(table));
    w
}

fn keyed(w: &World, sequence: u64, run: &str, key: &str) {
    let key = ProfileKey::parse(key).unwrap();
    w.fx.registry
        .queue_keyed_run(&host("a"), &lease(sequence), job(run), Some(key))
        .unwrap();
}

/// Catches: a mark that does not keep the end of the tail it was made with: the host's
/// one lease may run 900 s but its keyed run takes a minute at the P90, so the tail ends a
/// minute after the start; with no keyed run it ends at the bound.
#[test]
fn a_mark_keeps_the_end_of_the_tail_it_was_made_with() {
    let mut w = world();
    let start = w.now().millis();
    w.share(1, 900);
    keyed(&w, 1, "r1", "fast");
    w.ask(2, "q");
    assert_eq!(w.reservation("q").unwrap().tail_end, Some(start + 60_000));

    let mut bare = World::new(&["a"], plain);
    let start = bare.now().millis();
    bare.share(1, 900);
    bare.ask(2, "q");
    assert_eq!(
        bare.reservation("q").unwrap().tail_end,
        Some(start + 900_000)
    );
}

/// Catches: a tail end on a mark that drains nothing: a host that turns quiet is assigned
/// to the request, and the assigned mark has none.
#[test]
fn an_assigned_mark_has_no_tail_end() {
    let mut w = world();
    w.share(1, HOUR_S);
    w.ask(2, "q");
    assert!(w.reservation("q").unwrap().tail_end.is_some());
    w.step(60_000);
    w.stop(1);
    w.provider.observe(&lease(1));
    let mark = w.reservation("q").unwrap();
    assert_eq!((mark.assigned, mark.tail_end), (true, None));
}

/// Catches: the tail end left out of the ledger the registry hands the lease file, or
/// lost when the ledger is put back: the ledger row carries it, and a second registry
/// given the row marks the host with the same end.
#[test]
fn the_tail_end_goes_into_the_ledger_and_comes_back() {
    let mut w = world();
    let start = w.now().millis();
    w.share(1, 900);
    keyed(&w, 1, "r1", "fast");
    w.ask(2, "q");
    let ledger = w.fx.registry.reservation_ledger();
    let held = ledger[0].held.as_ref().expect("the row holds the host");
    assert_eq!(held.tail_end, Some(start + 60_000));

    let mut back = World::new(&["a"], plain);
    back.share(1, 900);
    back.step(w.now().millis() - back.now().millis());
    back.fx.registry.restore_reservations(&ledger);
    assert_eq!(
        back.reservation("q").map(|mark| mark.tail_end),
        Some(Some(start + 60_000))
    );
}

/// Catches: the backfill share counted wrongly over a scripted sequence: an ordinary lease
/// started before any mark counts nothing; a start refused on the marked host counts
/// nothing; the whole-node lease of the marked request, started on its host, is one start
/// on a marked host and not backfill.
#[test]
fn the_marked_requests_own_start_is_counted_and_is_not_backfill() {
    let mut w = world();
    w.share(1, HOUR_S);
    assert_eq!(w.fx.registry.tail_counters(), TailCounters::default());
    w.ask(2, "r1");
    assert_eq!(w.share(3, 60), no_capacity());
    assert_eq!(w.fx.registry.tail_counters(), TailCounters::default());
    w.stop(1);
    w.fx.registry.advance();
    w.poll(2, "r1", 600);
    assert_eq!(w.lease_on("a"), Some(lease(2)));
    let counters = w.fx.registry.tail_counters();
    assert_eq!((counters.started_on_marked, counters.backfilled), (1, 0));
}

/// Catches: the estimate error left out, signed wrongly or bucketed wrongly, and leases
/// with no estimate mixed into it: a lease estimated at two minutes ends at two and a
/// half, which is 30 s late and falls in the bucket above 10 s up to 60 s; a lease with no
/// keyed run ends and is counted apart; the stop of a lease the registry never started
/// and a second stop of an ended one count nothing.
#[test]
fn the_estimate_error_of_an_ended_lease_is_counted_by_sign_and_bucket() {
    let mut w = world();
    w.share(1, 900);
    keyed(&w, 1, "r1", "two");
    w.step(150_000);
    w.stop(1);
    let counters = w.fx.registry.tail_counters();
    assert_eq!(
        (counters.ended_estimated, counters.ended_unestimated),
        (1, 0)
    );
    assert_eq!(counters.error_sum_ms, 30_000);
    let late = ERROR_EDGES_MS.partition_point(|edge| *edge < 30_000);
    let mut buckets = [0; 8];
    buckets[late] = 1;
    assert_eq!(counters.error_buckets, buckets);

    w.share(2, 600);
    w.stop(2);
    w.stop(2);
    w.stop(99);
    let counters = w.fx.registry.tail_counters();
    assert_eq!(
        (counters.ended_estimated, counters.ended_unestimated),
        (1, 1)
    );
    assert_eq!(counters.error_buckets, buckets);
}
