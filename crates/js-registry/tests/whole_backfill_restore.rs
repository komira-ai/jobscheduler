//! What the placer does with a host set aside, after a restart: the mark comes back from
//! the reservation ledger with the tail it was made with, so a start that may backfill is
//! booked on that host only if it ends by that tail; a mark written without a tail (by a
//! release before tails were recorded) admits no backfill. The registry runs on a manual
//! clock, with the production placer (`js_sched_placer::place::SchedPlacer`).

mod support;

use std::time::Duration;

use js_lease::{Held, ReservationRecord};
use js_provider::{Pool, RefusalCode, StartAck, StartSpec};
use js_registry::{PoolConfig, Restored};
use js_types::Timestamp;
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

/// The first instant of every world.
const T0: u64 = 1_000_000;

/// The dedicated pool of the tests, which also serves `book`.
fn pool() -> PoolConfig {
    PoolConfig::new(
        ProviderId::new("builders").unwrap(),
        whole(),
        pool_shape(),
        4,
    )
    .with_whole_node(js_registry::WholeNodeConfig::dedicated(4))
    .unwrap()
    .with_share_class(book())
    .unwrap()
}

/// A registry started over host `a`, which carries `book` lease 1 of an hour started on
/// it, a minute later given a ledger in which request `q` holds `a` with `tail_end`.
fn restarted(tail_end: Option<u64>) -> World {
    let mut w = World::of(pool(), &["a"]);
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
    w
}

/// Catches: a restored mark whose tail is lost (no backfill after a
/// restart) or read as an absolute time, and a backfill that ends past the tail. The mark
/// was made with the tail at an hour; a minute later a start of a minute ends by it and is
/// booked on `a`, and one of an hour is not and is booked nowhere, as `a` is the only host.
#[test]
fn a_restored_mark_is_backfilled_only_by_work_that_ends_by_its_tail() {
    let mut w = restarted(Some(T0 + 3_600_000));
    assert_eq!(started_on(&w.provider.start(book_spec(2, 60))), Some("a"));
    let long = w.provider.start(book_spec(3, 3_600));
    assert_eq!(long, StartAck::Refused(RefusalCode::NoCapacity));
    assert_eq!(w.reserved_host("q"), Some("a".to_string()));
}

/// Catches: a mark restored without a tail read as a free host or as one
/// ending now: a start of a minute is booked nowhere.
#[test]
fn a_restored_mark_without_a_tail_takes_no_backfill() {
    let mut w = restarted(None);
    let ack = w.provider.start(book_spec(2, 60));
    assert_eq!(ack, StartAck::Refused(RefusalCode::NoCapacity));
    assert_eq!(w.reserved_host("q"), Some("a".to_string()));
}
