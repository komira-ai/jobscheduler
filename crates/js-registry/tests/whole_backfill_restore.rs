//! What a placer is shown of a host set aside, after a restart: the mark comes back from
//! the reservation ledger with the tail it was made with, so a start that may backfill is
//! shown the time left until that tail ends; a mark written without a tail (by a release
//! before tails were recorded) is shown with none, which admits no backfill. The registry
//! runs on a manual clock. The placer is a recording one, and js-sched's engine
//! (`--placement js-sched`), which books the start there only if it ends by that tail.

mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use js_lease::{Held, ReservationRecord, Timestamp};
use js_provider::Candidate;
use js_provider::{Pool, RefusalCode, StartAck, StartSpec};
use js_registry::{MarkedTails, Placer, PoolConfig, Restored};
use js_types::{Admission, ClassId, Dims, ProviderId, Request, ResourceClass, Resources};
use support::world::*;
use support::*;

fn book() -> ResourceClass {
    ResourceClass::new(
        ClassId::new("book").unwrap(),
        Admission::Book(Dims::new()),
        0,
    )
}

/// A `book` lease of one core that may run `wall_s`.
fn book_spec(sequence: u64, wall_s: u64) -> StartSpec {
    let request = Request {
        class: book().id().clone(),
        max_wall: Duration::from_secs(wall_s),
        resources: Resources::new(1000, 1 << 20, 0, None).unwrap(),
        ..request()
    };
    StartSpec::new(lease(sequence), request, &[book()]).unwrap()
}

/// A placer that books the first host no request has set aside, and keeps what it was
/// shown of the hosts set aside at each start.
#[derive(Default)]
struct Shown(Mutex<Vec<MarkedTails>>);

impl Placer for Shown {
    fn pick(&self, _: &StartSpec, open: &[Candidate], marked: &MarkedTails) -> Option<usize> {
        self.0.lock().unwrap().push(marked.clone());
        open.iter().position(|c| !marked.contains_key(&c.host))
    }
}

/// The first instant of every world.
const T0: u64 = 1_000_000;

/// The dedicated pool of the tests, which also serves `book`.
fn pool() -> PoolConfig {
    PoolConfig::new(
        ProviderId::new("builders").unwrap(),
        whole(),
        world_shape(),
        4,
    )
    .with_whole_node(js_registry::WholeNodeConfig::dedicated(4))
    .unwrap()
    .with_share_class(book())
    .unwrap()
}

/// A registry started over host `a`, which carries `book` lease 1 of an hour, given a
/// ledger in which request `q` holds `a` with `tail_end`, and the placer that records what
/// it is shown. The clock is a minute past the start.
fn restarted(tail_end: Option<u64>) -> (World, Arc<Shown>) {
    let mut w = World::of(pool(), &["a"]);
    let shown = Arc::new(Shown::default());
    w.fx.registry.set_placer(shown.clone());
    w.provider = w.fx.registry.provider();
    let w = restored(w, tail_end);
    shown.0.lock().unwrap().clear();
    (w, shown)
}

/// As [`restarted`], under `--placement js-sched`: the engine is the placer.
fn restarted_sched(tail_end: Option<u64>) -> World {
    let mut w = World::of_in(Placement::JsSched, pool(), &["a"]);
    w.provider = w.fx.registry.provider();
    restored(w, tail_end)
}

/// `w` with `book` lease 1 of an hour started on `a`, a minute later given a ledger in
/// which request `q` holds `a` with `tail_end`.
fn restored(mut w: World, tail_end: Option<u64>) -> World {
    assert_eq!(
        started_on(&w.provider.start(book_spec(1, 3_600))),
        Some("a")
    );
    w.step(60_000);
    let record = ReservationRecord {
        request_id: "q".to_string(),
        held: Some(Held {
            host: host("a"),
            reserved_at: Timestamp::from_millis(T0),
            deadline: Timestamp::from_millis(T0 + 3_600_000),
            assigned: false,
            tail_end,
        }),
        queued: Some(js_lease::Queued {
            place: 0,
            since: Timestamp::from_millis(T0),
        }),
        give_ups: 0,
        prepare_failures: 0,
        exclusions: Vec::new(),
        claim_by: None,
    };
    let restored = w.fx.registry.restore_reservations(&[record]);
    assert_eq!(
        restored,
        Restored {
            resumed: 1,
            dropped: 0
        }
    );
    w
}

/// Catches: a restored mark shown without its tail (no backfill after any restart), or
/// with the tail as an absolute time: the mark was made with the tail at an hour, and a
/// minute later a start is shown 59 minutes left on `a`.
#[test]
fn a_restored_mark_is_shown_with_the_time_left_until_its_persisted_tail() {
    let (mut w, shown) = restarted(Some(T0 + 3_600_000));
    w.provider.start(book_spec(2, 60));
    let seen = shown.0.lock().unwrap().clone();
    let left = Duration::from_secs(59 * 60);
    assert_eq!(seen, [MarkedTails::from([(host("a"), Some(left))])]);
}

/// Catches: a ledger written under a placer that keeps no whole-node queue (one that takes
/// the trait's defaults) without the registry's own mark and place: the record a restart put
/// back is written again whole, with its tail.
#[test]
fn a_placer_that_keeps_no_queue_leaves_the_ledger_to_the_registry() {
    let (w, _) = restarted(Some(T0 + 3_600_000));
    let ledger = w.fx.registry.reservation_ledger();
    assert_eq!(ledger.len(), 1);
    let row = &ledger[0];
    assert_eq!(row.request_id, "q");
    let held = row.held.as_ref().unwrap();
    assert_eq!(
        (held.host.as_str(), held.tail_end),
        ("a", Some(T0 + 3_600_000))
    );
    assert_eq!(held.reserved_at, Timestamp::from_millis(T0));
    let queued = row.queued.unwrap();
    assert_eq!(
        (queued.place, queued.since),
        (0, Timestamp::from_millis(T0))
    );
    assert_eq!((row.give_ups, row.prepare_failures), (0, 0));
    assert!(row.exclusions.is_empty());
}

/// Catches: a mark restored without a tail read as a free host, or as one ending now: the
/// start is shown `a` with no known tail, and the placer (which books no marked host)
/// books nothing on it.
#[test]
fn a_restored_mark_without_a_tail_is_shown_with_none() {
    let (mut w, shown) = restarted(None);
    let ack = w.provider.start(book_spec(2, 60));
    let seen = shown.0.lock().unwrap().clone();
    assert_eq!(seen, [MarkedTails::from([(host("a"), None)])]);
    assert_eq!(ack, StartAck::Refused(RefusalCode::NoCapacity));
}

/// Catches: under js-sched, a restored mark whose tail is lost (no backfill after a
/// restart) or read as an absolute time, and a backfill that ends past the tail. The mark
/// was made with the tail at an hour; a minute later a start of a minute ends by it and is
/// booked on `a`, and one of an hour is not and is booked nowhere, as `a` is the only host.
#[test]
fn under_js_sched_a_restored_mark_is_backfilled_only_by_work_that_ends_by_its_tail() {
    let mut w = restarted_sched(Some(T0 + 3_600_000));
    assert_eq!(started_on(&w.provider.start(book_spec(2, 60))), Some("a"));
    let long = w.provider.start(book_spec(3, 3_600));
    assert_eq!(long, StartAck::Refused(RefusalCode::NoCapacity));
    assert_eq!(w.reserved_host("q"), Some("a".to_string()));
}

/// Catches: under js-sched, a mark restored without a tail read as a free host or as one
/// ending now: a start of a minute is booked nowhere.
#[test]
fn under_js_sched_a_restored_mark_without_a_tail_takes_no_backfill() {
    let mut w = restarted_sched(None);
    let ack = w.provider.start(book_spec(2, 60));
    assert_eq!(ack, StartAck::Refused(RefusalCode::NoCapacity));
    assert_eq!(w.reserved_host("q"), Some("a".to_string()));
}

/// Catches: backfill with no placer. A registry over the same pool without one refuses
/// the `book` lease as an unsupported class, so a restored mark takes nothing.
#[test]
fn without_a_placer_a_class_that_shares_takes_no_host() {
    let pool = PoolConfig::new(
        ProviderId::new("builders").unwrap(),
        whole(),
        pool_shape(),
        4,
    )
    .with_share_class(book())
    .unwrap();
    let mut w = World::of(pool, &["a"]);
    w.provider = w.fx.registry.provider();
    let ack = w.provider.start(book_spec(1, 60));
    assert_eq!(ack, StartAck::Refused(RefusalCode::UnsupportedClass));
}
