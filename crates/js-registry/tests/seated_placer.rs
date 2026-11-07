//! What the registry tells its placer of a lease put back at a start
//! ([`js_registry::Placer::seated`]): the host, what the lease books, the start and end
//! bound the lease file records, and whether it is a whole-node lease; and nothing for a
//! put-back the registry refuses. The placer is a recorder; the clock is the test's.

mod support;

use std::sync::{Arc, Mutex};

use js_lease::Timestamp;
use js_provider::{Candidate, Held, SeatError, StartSpec};
use js_registry::{LeaseTimes, MarkedTails, Placer, SeatedLease};
use support::world::*;
use support::*;

/// A placer that books the first host it is shown and records every lease it is told was
/// seated.
#[derive(Default)]
struct Recorder(Mutex<Vec<SeatedLease>>);

impl Placer for Recorder {
    fn pick(&self, _: &StartSpec, open: &[Candidate], _: &MarkedTails) -> Option<usize> {
        (!open.is_empty()).then_some(0)
    }

    fn seated(&self, seated: &SeatedLease) {
        self.0.lock().unwrap().push(seated.clone());
    }
}

/// `world` with a recorder set as its placer.
fn recorded(world: &World) -> Arc<Recorder> {
    let recorder = Arc::new(Recorder::default());
    world.fx.registry.set_placer(recorder.clone());
    recorder
}

fn told(recorder: &Recorder) -> Vec<SeatedLease> {
    recorder.0.lock().unwrap().clone()
}

/// Asserts that a lease of a whole-node pool put back from the lease file is told to the
/// placer as a whole-node lease booking the whole host, with the start and bound the file
/// records, on a host that has said hello and on one that has not; and that a second lease
/// put back on a held host, which the registry refuses, is not told. It catches the times
/// dropped on the way to the placer, a whole-node lease told as one a request may drain,
/// and a refused put-back the placer would book.
#[test]
fn a_whole_node_lease_put_back_is_told_with_its_times() {
    let w = World::new(&["a"], |settings| settings);
    let recorder = recorded(&w);
    let booking = booking_at("a", 1, None, 1_000, 61_000);
    assert_eq!(w.provider.seat_booked(&booking, None), Ok(()));
    let unheard = booking_at("z", 2, None, 2_000, 3_000);
    assert_eq!(w.provider.seat_booked(&unheard, None), Ok(()));
    let taken = booking_at("a", 3, None, 0, 0);
    let refused = w.provider.seat_booked(&taken, None);
    assert!(
        matches!(refused, Err(SeatError::Taken { .. })),
        "{refused:?}"
    );
    let seated = |on: &str, sequence: u64, (started, ends): (u64, u64)| SeatedLease {
        host: host(on),
        held: Held {
            lease: lease(sequence),
            resources: world_shape().resources().clone(),
            whole: true,
        },
        times: Some(LeaseTimes {
            started_at: Timestamp::from_millis(started),
            ends_at: Timestamp::from_millis(ends),
        }),
        whole_node: true,
    };
    let expected = [
        seated("a", 1, (1_000, 61_000)),
        seated("z", 2, (2_000, 3_000)),
    ];
    assert_eq!(told(&recorder), expected);
}

/// Asserts that a lease of a pool whose class books the whole host but is not the
/// whole-node class, put back with no times (`RegisteredProvider::seat`), is told with
/// none and as no whole-node lease, and that one sharing its host is told with what it
/// books. It catches times made up where the caller gave none, and every lease that books
/// a whole host told as a whole-node lease (a whole-node request could then drain none).
#[test]
fn a_lease_put_back_without_times_is_told_with_none() {
    let book = js_types::ResourceClass::new(
        js_types::ClassId::new("book").unwrap(),
        js_types::Admission::Book(js_types::Dims::new()),
        0,
    );
    let w = World::of(world_config(4).with_share_class(book).unwrap(), &["a"]);
    let recorder = recorded(&w);
    assert_eq!(w.provider.seat(&host("a"), lease(1), None), Ok(()));
    let shares = js_types::Resources::new(1000, 1 << 20, 0, None).unwrap();
    let shared = booking_at("b", 2, Some(shares.clone()), 5, 10);
    assert_eq!(w.provider.seat_booked(&shared, None), Ok(()));
    let told = told(&recorder);
    let whole = Held {
        lease: lease(1),
        resources: world_shape().resources().clone(),
        whole: true,
    };
    assert_eq!(
        (told[0].held.clone(), told[0].times, told[0].whole_node),
        (whole, None, false)
    );
    let share = Held {
        lease: lease(2),
        resources: shares,
        whole: false,
    };
    assert_eq!((told[1].held.clone(), told[1].whole_node), (share, false));
    assert_eq!(told.len(), 2);
}
