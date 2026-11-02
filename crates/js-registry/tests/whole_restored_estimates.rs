//! The estimate feed over leases put back from the lease file
//! ([`js_registry::RegisteredProvider::seat_booked`]): with a source of lives set, each
//! lease keeps the start and the bound the file records, and its tail and its stop are
//! counted from them. Each test reads the exact tail a reservation was made with, on a
//! manual clock; the profile lives are a fixed table.

mod support;

use std::collections::BTreeMap;
use std::sync::Arc;

use js_lease::Timestamp;
use js_pool_static::{Candidate, SeatError};
use js_provider::{Provider, StartSpec};
use js_registry::{MarkedTails, Placer, ProfileLives, TailCounters};
use js_types::{Admission, ClassId, Dims, Request, ResourceClass, Resources};
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

/// A placer that books the first host it is shown that no whole-node request has set aside.
struct First;

impl Placer for First {
    fn pick(&self, _: &StartSpec, open: &[Candidate], marked: &MarkedTails) -> Option<usize> {
        open.iter().position(|c| !marked.contains_key(&c.host))
    }
}

fn book() -> ResourceClass {
    ResourceClass::new(
        ClassId::new("book").unwrap(),
        Admission::Book(Dims::new()),
        0,
    )
}

/// `hosts` in a pool that books whole hosts (so a lease put back is a plain booking),
/// serving `book` leases that share a host and whole-node requests, with the table of lives
/// set when `lives` is.
fn world(hosts: &[&str], lives: bool) -> World {
    let mut w = World::of(config(4), hosts);
    w.fx.registry.set_placer(Arc::new(First));
    w.provider = w.provider.clone().with_classes(vec![whole(), book()]);
    if lives {
        let table = BTreeMap::from([("fast", 1), ("three", 3), ("slow", 8)]);
        w.fx.registry.set_lives(Arc::new(Table(table)));
    }
    w
}

/// One core and 1 MiB: what a `book` lease books.
fn core() -> Resources {
    Resources::new(1000, 1 << 20, 0, None).unwrap()
}

/// Puts back lease `sequence` on `on` as the lease file records it: started at `started`
/// and bound to end at `ends` (milliseconds on the registry's clock), booking `shares`.
fn restore(
    w: &World,
    on: &str,
    sequence: u64,
    shares: Option<Resources>,
    (started, ends): (u64, u64),
) -> Result<(), SeatError> {
    let booking = booking_at(on, sequence, shares, started, ends);
    w.provider.seat_booked(&booking, None)
}

/// Queues run `run` of lease `sequence` on `on`, named by profile `key`.
fn keyed(w: &World, on: &str, sequence: u64, run: &str, key: &str) {
    let key = ProfileKey::parse(key).unwrap();
    w.fx.registry
        .queue_keyed_run(&host(on), &lease(sequence), job(run), Some(key))
        .unwrap();
}

/// Asks for a whole node as `name` under lease `sequence`, and returns the host reserved
/// for it and the tail it was reserved with.
fn ask(w: &mut World, sequence: u64, name: &str) -> (Option<String>, Option<u64>) {
    w.ask(sequence, name);
    let reservation = w.reservation(name);
    let host = reservation.as_ref().map(|r| r.host.to_string());
    (host, reservation.and_then(|r| r.tail_end))
}

/// Withdraws request `name`, so a later request is reserved afresh.
fn withdraw(w: &mut World, name: &str) {
    assert!(w.cancel(name));
}

const MINUTE: u64 = 60_000;

/// Catches: the bounds of a shared host's restored leases mixed up, a lease's start taken
/// from its sibling, a sibling's bound dropped when another lease of the host stops, and a
/// lease started after a restore not counted beside the restored ones. `a` carries two
/// restored shares: lease 1, started two minutes before now, bound to 15 minutes, with a
/// keyed run of three minutes (so it ends a minute from now), and lease 2, started a minute
/// before now, bound to five minutes, with no keyed run (so it ends four minutes from now).
/// The tail is lease 2's end; once lease 2 stops it is lease 1's estimate; a lease of ten
/// minutes started after that is the tail.
#[test]
fn restored_sharing_leases_each_keep_their_own_start_and_bound() {
    let mut w = world(&["a"], true);
    let now = w.now().millis();
    let first = (now - 2 * MINUTE, now - 2 * MINUTE + 15 * MINUTE);
    let second = (now - MINUTE, now - MINUTE + 5 * MINUTE);
    assert_eq!(restore(&w, "a", 1, Some(core()), first), Ok(()));
    assert_eq!(restore(&w, "a", 2, Some(core()), second), Ok(()));
    keyed(&w, "a", 1, "r1", "three");
    let tail = ask(&mut w, 10, "q1");
    assert_eq!(tail, (Some("a".to_string()), Some(now + 4 * MINUTE)));
    withdraw(&mut w, "q1");
    w.stop(2);
    let tail = ask(&mut w, 11, "q2");
    assert_eq!(tail, (Some("a".to_string()), Some(now + MINUTE)));
    withdraw(&mut w, "q2");
    let spec = StartSpec::new(
        lease(3),
        Request {
            class: book().id().clone(),
            resources: core(),
            max_wall: std::time::Duration::from_secs(600),
            ..request()
        },
        &[book()],
    );
    assert!(started_on(&w.provider.start(spec.unwrap())).is_some());
    let tail = ask(&mut w, 12, "q3");
    assert_eq!(tail, (Some("a".to_string()), Some(now + 10 * MINUTE)));
}

/// Catches: a lease put back a second time keeping the start and bound of the first. Lease
/// 1 is put back as started now with a bound of 15 minutes, then again as started a minute
/// before now with a bound of five minutes: the tail is the second bound, and with a keyed
/// run of three minutes, two minutes from now (counted from the second start).
#[test]
fn a_lease_put_back_again_takes_the_start_and_bound_of_the_second_seat() {
    let mut w = world(&["a"], true);
    let now = w.now().millis();
    assert_eq!(restore(&w, "a", 1, None, (now, now + 15 * MINUTE)), Ok(()));
    let again = (now - MINUTE, now + 4 * MINUTE);
    assert_eq!(restore(&w, "a", 1, None, again), Ok(()));
    let tail = ask(&mut w, 10, "q1");
    assert_eq!(tail, (Some("a".to_string()), Some(now + 4 * MINUTE)));
    withdraw(&mut w, "q1");
    keyed(&w, "a", 1, "r1", "three");
    let tail = ask(&mut w, 11, "q2");
    assert_eq!(tail, (Some("a".to_string()), Some(now + 2 * MINUTE)));
}

/// Catches: a refused seat that still keeps a start or a bound, or counts a stop. With the
/// feed on, lease 1 is put back on `a` with a bound of five minutes, and every refusal of
/// `seat_booked` is met in turn: another lease on `a` (`Taken`), lease 1 on `b`
/// (`Elsewhere`), a share larger than the empty host `b` (`NoRoom`), and lease 1 again
/// once it is stopped (`Ended`). Each refused booking carries a bound of an hour; the tail
/// of `a` stays five minutes while lease 1 is booked, and the counters show only lease 1's
/// stop.
#[test]
fn a_refused_seat_keeps_no_start_or_bound_and_counts_nothing() {
    let mut w = world(&["a", "b"], true);
    let now = w.now().millis();
    let hour = (now, now + 60 * MINUTE);
    assert_eq!(restore(&w, "a", 1, None, (now, now + 5 * MINUTE)), Ok(()));
    let taken = SeatError::Taken {
        host: host("a"),
        holder: lease(1),
    };
    assert_eq!(restore(&w, "a", 2, None, hour), Err(taken));
    let elsewhere = SeatError::Elsewhere {
        lease: lease(1),
        host: host("a"),
    };
    assert_eq!(restore(&w, "b", 1, None, hour), Err(elsewhere));
    let huge = Resources::new(5000, 1 << 20, 0, None).unwrap();
    assert_eq!(
        restore(&w, "b", 3, Some(huge), hour),
        Err(SeatError::NoRoom(host("b")))
    );
    assert_eq!(restore(&w, "b", 4, None, (now, now + 10 * MINUTE)), Ok(()));
    let tail = ask(&mut w, 10, "q1");
    assert_eq!(tail, (Some("a".to_string()), Some(now + 5 * MINUTE)));
    withdraw(&mut w, "q1");
    assert_eq!(w.fx.registry.tail_counters(), TailCounters::default());
    w.stop(1);
    assert_eq!(
        restore(&w, "a", 1, None, hour),
        Err(SeatError::Ended(lease(1)))
    );
    let counted = w.fx.registry.tail_counters();
    let one_unestimated = TailCounters {
        ended_unestimated: 1,
        ..TailCounters::default()
    };
    assert_eq!(counted, one_unestimated);
}

/// Catches: the stop of a restored lease counted from the restart, or counted twice. Lease
/// 1 started two minutes before the restart and its keyed run takes three, so it is
/// estimated to end a minute after now; it stops at ninety seconds, half a minute late:
/// one estimated end, an error of +30 000 ms in the (10 s, 60 s] bucket. Counted from the
/// restart the error would be -90 000 ms. A second stop counts nothing.
#[test]
fn the_stop_of_a_restored_lease_counts_its_error_from_the_recorded_start_once() {
    let mut w = world(&["a"], true);
    let now = w.now().millis();
    let started = now - 2 * MINUTE;
    assert_eq!(
        restore(&w, "a", 1, None, (started, started + 15 * MINUTE)),
        Ok(())
    );
    keyed(&w, "a", 1, "r1", "three");
    w.step(90_000);
    w.stop(1);
    let mut buckets = [0; 8];
    buckets[5] = 1;
    let expected = TailCounters {
        ended_estimated: 1,
        error_sum_ms: 30_000,
        error_buckets: buckets,
        ..TailCounters::default()
    };
    assert_eq!(w.fx.registry.tail_counters(), expected);
    w.stop(1);
    assert_eq!(w.fx.registry.tail_counters(), expected);
}

/// Catches: a restored start ahead of the registry's clock (the clock stepped back since
/// the file was written) clamped to now or refused. Lease 1 is recorded as started a minute
/// from now with a keyed run of three minutes, so it ends four minutes from now, inside its
/// 15 minute bound.
#[test]
fn a_restored_start_ahead_of_the_clock_is_counted_as_recorded() {
    let mut w = world(&["a"], true);
    let now = w.now().millis();
    let ahead = (now + MINUTE, now + MINUTE + 15 * MINUTE);
    assert_eq!(restore(&w, "a", 1, None, ahead), Ok(()));
    keyed(&w, "a", 1, "r1", "three");
    let tail = ask(&mut w, 10, "q1");
    assert_eq!(tail, (Some("a".to_string()), Some(now + 4 * MINUTE)));
}

/// Catches: with the feed on, a lease whose recorded end saturated (an endless max wall)
/// taken for a far but known bound. With no keyed run its tail is unknown, so its host
/// ranks after `b` (ten minutes) and, alone, is reserved with no tail; a keyed run of one
/// minute makes it known, ending a minute after its start. With the feed off the same lease
/// keeps the saturated bound as before (and, restored, has none at all).
#[test]
fn with_the_feed_on_a_saturated_end_is_no_bound_and_the_tail_is_unknown_until_a_keyed_run() {
    let mut w = world(&["a", "b"], true);
    let now = w.now().millis();
    let endless = (now, Timestamp::MAX.millis());
    assert_eq!(restore(&w, "a", 1, None, endless), Ok(()));
    assert_eq!(restore(&w, "b", 2, None, (now, now + 10 * MINUTE)), Ok(()));
    let tail = ask(&mut w, 10, "q1");
    assert_eq!(tail, (Some("b".to_string()), Some(now + 10 * MINUTE)));
    withdraw(&mut w, "q1");
    w.stop(2);
    let mut alone = world(&["a"], true);
    assert_eq!(restore(&alone, "a", 1, None, endless), Ok(()));
    assert_eq!(ask(&mut alone, 10, "q2"), (Some("a".to_string()), None));
    withdraw(&mut alone, "q2");
    keyed(&alone, "a", 1, "r1", "fast");
    let tail = ask(&mut alone, 11, "q3");
    assert_eq!(tail, (Some("a".to_string()), Some(now + MINUTE)));
}
