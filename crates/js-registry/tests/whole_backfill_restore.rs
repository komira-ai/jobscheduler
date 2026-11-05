//! What a placer is shown of a host set aside, after a restart: the mark comes back from
//! the reservation ledger with the tail it was made with, so a start that may backfill is
//! shown the time left until that tail ends; a mark written without a tail (by a release
//! before tails were recorded) is shown with none, which admits no backfill. The registry
//! runs on a manual clock.

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

/// A registry started over host `a`, which carries `book` lease 1 of an hour, given a
/// ledger in which request `q` holds `a` with `tail_end`, and the placer that records what
/// it is shown. The clock is a minute past the start.
fn restarted(tail_end: Option<u64>) -> (World, Arc<Shown>) {
    let pool = PoolConfig::new(
        ProviderId::new("builders").unwrap(),
        whole(),
        pool_shape(),
        4,
    )
    .with_whole_node(js_registry::WholeNodeConfig::dedicated(4))
    .unwrap()
    .with_share_class(book())
    .unwrap();
    let mut w = World::of(pool, &["a"]);
    let shown = Arc::new(Shown::default());
    w.fx.registry.set_placer(shown.clone());
    w.provider = w.fx.registry.provider();
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
    shown.0.lock().unwrap().clear();
    (w, shown)
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
