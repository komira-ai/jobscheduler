//! The estimate feed: a lease's expected end, from the start of the lease and the P90 of
//! the profiles of its keyed runs, reaches the drain cost, so the host reserved for a
//! whole-node request is the one that empties first by estimate. The registry is driven
//! on a manual clock; the profile lives are a fixed table. Each test runs under both
//! placements, `--placement registry` (`registry::NAME`) and `--placement js-sched`
//! (`js_sched::NAME`), with the same assertions, except the three of a lease put back
//! from the lease file, which run under `--placement registry` alone
//! (`registry_only::NAME`): js-sched's engine is not told of a lease put back at a
//! restart yet (komira-ai/jobscheduler#778).

mod support;

use std::collections::BTreeMap;
use std::sync::Arc;

use js_provider::Pool;
use js_registry::ProfileLives;
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

fn table() -> Arc<Table> {
    Arc::new(Table(BTreeMap::from([
        ("fast", 1),
        ("three", 3),
        ("slow", 8),
        ("long", 600),
    ])))
}

/// Hosts `a` and `b` with the table of lives set.
fn world(p: Placement) -> World {
    let w = World::new_in(p, &["a", "b"], plain);
    w.fx.registry.set_lives(table());
    w
}

/// Queues a run of lease `sequence` on `on`, named by profile `key`.
fn keyed(w: &World, on: &str, sequence: u64, run: &str, key: &str) {
    let key = ProfileKey::parse(key).unwrap();
    w.fx.registry
        .queue_keyed_run(&host(on), &lease(sequence), job(run), Some(key))
        .unwrap();
}

/// Catches: no estimate fed: `a`'s lease may run 900 s but its keyed run takes a minute
/// at the P90, so `a` empties before `b` (600 s) and is the host reserved; by bounds
/// alone it would be `b`.
fn a_short_estimate_makes_the_host_cheaper_than_a_shorter_bound(p: Placement) {
    let mut w = world(p);
    w.share(1, 900);
    w.share(2, 600);
    keyed(&w, "a", 1, "r1", "fast");
    w.ask(3, "q");
    assert_eq!(w.reserved_host("q"), Some("a".to_string()));
}

/// Catches: the order changed for a lease with no keyed run: it is costed at its bound,
/// so `b` (600 s) is reserved over `a` (900 s).
fn a_lease_with_no_keyed_run_is_costed_at_its_bound(p: Placement) {
    let mut w = world(p);
    w.share(1, 900);
    w.share(2, 600);
    w.ask(3, "q");
    assert_eq!(w.reserved_host("q"), Some("b".to_string()));
}

/// Catches: a key with no P90 read as an instant end: `a`'s run names a profile the table
/// does not know, so `a` is costed at its 900 s bound and `b` is reserved.
fn a_key_with_no_p90_leaves_the_lease_at_its_bound(p: Placement) {
    let mut w = world(p);
    w.share(1, 900);
    w.share(2, 600);
    keyed(&w, "a", 1, "r1", "unseen");
    w.ask(3, "q");
    assert_eq!(w.reserved_host("q"), Some("b".to_string()));
}

/// Catches: the smallest or the first P90 taken in place of the largest: `a` runs a one
/// minute profile and an eight minute one, so it ends at 480 s, after `b` (300 s).
fn several_keyed_runs_cost_the_lease_at_the_largest_p90(p: Placement) {
    let mut w = world(p);
    w.share(1, 900);
    w.share(2, 300);
    keyed(&w, "a", 1, "r1", "fast");
    keyed(&w, "a", 1, "r2", "slow");
    w.ask(3, "q");
    assert_eq!(w.reserved_host("q"), Some("b".to_string()));
}

/// Catches: an estimate past the bound taking the lease past its max wall: `a`'s profile
/// takes 600 minutes but the lease may run 300 s, so it ends at 300 s, before `b` (400 s).
fn an_estimate_beyond_the_bound_is_cut_to_the_bound(p: Placement) {
    let mut w = world(p);
    w.share(1, 300);
    w.share(2, 400);
    keyed(&w, "a", 1, "r1", "long");
    w.ask(3, "q");
    assert_eq!(w.reserved_host("q"), Some("a".to_string()));
}

/// Catches: the estimate counted from the time the run was queued instead of from the
/// start of the lease: both leases start together, `a`'s run is queued two minutes on
/// and takes three, so `a` ends at 180 s, before `b` (250 s); counted from the queueing
/// it would end at 300 s, after it.
fn the_estimate_counts_from_the_start_of_the_lease(p: Placement) {
    let mut w = world(p);
    w.share(1, 900);
    w.share(2, 250);
    w.step(120_000);
    keyed(&w, "a", 1, "r1", "three");
    w.ask(3, "q");
    assert_eq!(w.reserved_host("q"), Some("a".to_string()));
}

/// Catches: keyed runs costed without any source of lives set: the registry falls back to
/// the bounds, so `b` (600 s) is reserved over `a` (900 s).
fn with_no_source_of_lives_a_keyed_run_changes_nothing(p: Placement) {
    let mut w = World::new_in(p, &["a", "b"], plain);
    w.share(1, 900);
    w.share(2, 600);
    keyed(&w, "a", 1, "r1", "fast");
    w.ask(3, "q");
    assert_eq!(w.reserved_host("q"), Some("b".to_string()));
}

/// The wall time of a lease with no max-wall bound: its end saturates to the end of time.
const NO_WALL_S: u64 = u64::MAX / 1000;

/// Catches: a lease with no keyed run and no bound ranked by what is known of its host, or
/// first, or its saturated end taken for a far but known bound. `a`'s lease may run for
/// ever (its end saturates) and has no keyed run, so with the feed on its tail is unknown:
/// `a` ranks after `b`, whose lease is bound to end in ten hours, and alone it is reserved
/// with no tail. Once a keyed run gives the lease a one minute estimate, `a` is known, ends
/// a minute after the lease's start and empties first.
fn a_lease_with_no_keyed_run_and_no_bound_is_unknown_and_ranks_after_every_known_tail(
    p: Placement,
) {
    let mut w = world(p);
    let start = w.now().millis();
    w.share(1, NO_WALL_S);
    w.share(2, 36_000);
    w.ask(3, "q");
    assert_eq!(w.reserved_host("q"), Some("b".to_string()));
    assert_eq!(tail_of(&w, "q"), Some(start + 36_000_000));
    w.cancel("q");
    keyed(&w, "a", 1, "r1", "fast");
    w.ask(4, "q2");
    assert_eq!(w.reserved_host("q2"), Some("a".to_string()));
    assert_eq!(tail_of(&w, "q2"), Some(start + 60_000));

    let mut alone = World::new_in(p, &["a"], plain);
    alone.fx.registry.set_lives(table());
    alone.share(1, NO_WALL_S);
    alone.ask(3, "q");
    assert_eq!(alone.reserved_host("q"), Some("a".to_string()));
    assert_eq!(tail_of(&alone, "q"), None);
}

/// Catches: the flag-off state changing for a lease whose end saturates. With no source of
/// lives the registry's pass keeps such a lease's saturated end as a far, known bound, as
/// before the feed. js-sched's engine has no flag-off state: it costs the lease by its
/// start and wall time, whose end saturates, and a saturated end is an unknown tail, as
/// with the feed on (the `alone` case above).
fn without_a_source_of_lives_a_saturated_end_stays_a_far_bound(p: Placement) {
    let mut w = World::new_in(p, &["a"], plain);
    w.share(1, NO_WALL_S);
    w.ask(3, "q");
    assert_eq!(w.reserved_host("q"), Some("a".to_string()));
    let tail = match p {
        Placement::Registry => Some(js_lease::Timestamp::MAX.millis()),
        Placement::JsSched => None,
    };
    assert_eq!(tail_of(&w, "q"), tail);
}

/// The tail request `name` was reserved with.
fn tail_of(w: &World, name: &str) -> Option<u64> {
    w.reservation(name).and_then(|r| r.tail_end)
}

/// Hosts `a` and `b` in a pool that books whole hosts, so a lease put back is a plain
/// booking, with the table of lives set when `lives` is.
fn restored_world(p: Placement, lives: bool) -> World {
    let w = World::of_in(p, world_config(4), &["a", "b"]);
    if lives {
        w.fx.registry.set_lives(table());
    }
    w
}

/// Puts back lease `sequence` on `on`, as the lease file records it: started `ago_ms`
/// before now and bound to end `wall_ms` after its start.
fn restore(w: &World, on: &str, sequence: u64, ago_ms: u64, wall_ms: u64) {
    let started = w.now().millis() - ago_ms;
    let booking = booking_at(on, sequence, None, started, started.saturating_add(wall_ms));
    w.provider.seat_booked(&booking, None).unwrap();
}

/// Catches: a lease put back after a restart counted from the time it was put back, or as
/// ending now. `a`'s lease started two minutes before the restart and its keyed run takes
/// three, so it ends in a minute, before `b`'s lease (two minutes); counted from the
/// restart it would end in three, after `b`.
fn a_restored_lease_is_costed_from_the_start_the_lease_file_records(p: Placement) {
    let mut w = restored_world(p, true);
    restore(&w, "a", 1, 120_000, 900_000);
    w.share(2, 120);
    keyed(&w, "a", 1, "r1", "three");
    w.ask(3, "q");
    assert_eq!(w.reserved_host("q"), Some("a".to_string()));
}

/// Catches: a lease put back with no bound, which would count as ending now. `a`'s lease
/// has no keyed run and is bound to end 900 s after its start, so it falls back to that
/// bound and `b` (600 s) is reserved.
fn a_restored_lease_with_no_keyed_run_is_costed_at_its_recorded_bound(p: Placement) {
    let mut w = restored_world(p, true);
    restore(&w, "a", 1, 0, 900_000);
    w.share(2, 600);
    w.ask(3, "q");
    assert_eq!(w.reserved_host("q"), Some("b".to_string()));
}

/// Catches: the start and bound of a restored lease kept with the estimate feed off.
/// Without a source of lives a lease put back has no bound, as it always had, and counts
/// as ending now, so `a` is reserved over `b` (600 s) whatever the file records, and its
/// stop counts nothing.
fn without_a_source_of_lives_a_restored_lease_counts_as_ending_now(p: Placement) {
    let mut w = restored_world(p, false);
    restore(&w, "a", 1, 0, 900_000);
    w.share(2, 600);
    w.ask(3, "q");
    assert_eq!(w.reserved_host("q"), Some("a".to_string()));
    w.provider.stop(&lease(1));
    let counted = w.fx.registry.tail_counters();
    assert_eq!((counted.ended_estimated, counted.ended_unestimated), (0, 0));
}

/// Catches: a lease put back twice holding two bounds, which would count its stop twice.
/// With the feed on, the stop of a restored lease with no keyed run counts once as ended
/// with no estimate.
fn a_lease_put_back_twice_keeps_one_bound_and_its_stop_counts_once(p: Placement) {
    let mut w = restored_world(p, true);
    restore(&w, "a", 1, 0, 900_000);
    restore(&w, "a", 1, 0, 900_000);
    w.provider.stop(&lease(1));
    let counted = w.fx.registry.tail_counters();
    assert_eq!((counted.ended_estimated, counted.ended_unestimated), (0, 1));
}

crate::both!(
    a_short_estimate_makes_the_host_cheaper_than_a_shorter_bound,
    a_lease_with_no_keyed_run_is_costed_at_its_bound,
    a_key_with_no_p90_leaves_the_lease_at_its_bound,
    several_keyed_runs_cost_the_lease_at_the_largest_p90,
    an_estimate_beyond_the_bound_is_cut_to_the_bound,
    the_estimate_counts_from_the_start_of_the_lease,
    with_no_source_of_lives_a_keyed_run_changes_nothing,
    a_lease_with_no_keyed_run_and_no_bound_is_unknown_and_ranks_after_every_known_tail,
    without_a_source_of_lives_a_saturated_end_stays_a_far_bound,
    a_lease_put_back_twice_keeps_one_bound_and_its_stop_counts_once,
);

/// The tests of the drain cost of a lease put back from the lease file, under
/// `--placement registry` alone until js-sched's engine is told of such a lease
/// (komira-ai/jobscheduler#778).
mod registry_only {
    use super::support::Placement;

    #[test]
    fn a_restored_lease_is_costed_from_the_start_the_lease_file_records() {
        super::a_restored_lease_is_costed_from_the_start_the_lease_file_records(
            Placement::Registry,
        );
    }

    #[test]
    fn a_restored_lease_with_no_keyed_run_is_costed_at_its_recorded_bound() {
        super::a_restored_lease_with_no_keyed_run_is_costed_at_its_recorded_bound(
            Placement::Registry,
        );
    }

    #[test]
    fn without_a_source_of_lives_a_restored_lease_counts_as_ending_now() {
        super::without_a_source_of_lives_a_restored_lease_counts_as_ending_now(Placement::Registry);
    }
}
