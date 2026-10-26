//! Behaviour of the fake provider: placement, scripted Unknown answers and defects.

use std::collections::BTreeMap;
use std::time::Duration;

use js_provider::{Call, Flavour, Observed, Provider, RefusalCode, StartAck, StartSpec, StopAck};
use js_provider_fake::{Defect, FakeProvider};
use js_types::{ClassId, HostId, LeaseId, ProviderId, Request, ResourceClass, Resources, Shape};

fn host(name: &str) -> HostId {
    HostId::new(name).unwrap()
}

fn fake() -> FakeProvider {
    let shape = Shape::new(
        Resources::new(8000, 8, 1, Some("g".into())).unwrap(),
        BTreeMap::new(),
    );
    let hosts = vec![(host("h1"), shape.clone()), (host("h2"), shape)];
    FakeProvider::new(ProviderId::new("fake").unwrap(), hosts)
}

fn spec(seq: u64, cpu_millis: u64) -> StartSpec {
    let request = Request {
        class: ClassId::new("gpu-host").unwrap(),
        resources: Resources::new(cpu_millis, 1, 1, Some("g".into())).unwrap(),
        capabilities: BTreeMap::new(),
        max_wall: Duration::from_secs(60),
        labels: BTreeMap::new(),
        validation_run: None,
    };
    StartSpec::new(LeaseId::new(1, seq), request, &[ResourceClass::gpu_host()]).unwrap()
}

fn lease(seq: u64) -> LeaseId {
    LeaseId::new(1, seq)
}

/// Asserts that the fake books the first free fitting host, is idempotent on the
/// lease, refuses when full, and frees on stop.
#[test]
fn books_idempotently_and_frees_on_stop() {
    let mut f = fake();
    assert_eq!(f.id().as_str(), "fake");
    assert_eq!(f.flavour(), Flavour::Delegated);
    assert_eq!(f.classes(), vec![ResourceClass::gpu_host()]);
    assert_eq!(
        f.start(spec(1, 1000)),
        StartAck::Started { host: host("h1") }
    );
    assert_eq!(
        f.start(spec(1, 1000)),
        StartAck::Started { host: host("h1") }
    );
    assert_eq!(
        f.start(spec(2, 1000)),
        StartAck::Started { host: host("h2") }
    );
    assert_eq!(
        f.start(spec(3, 1000)),
        StartAck::Refused(RefusalCode::NoCapacity)
    );
    assert_eq!(
        f.start(spec(4, 99_000)),
        StartAck::Refused(RefusalCode::NoCapacity)
    );
    assert_eq!(f.booked().len(), 2);
    assert_eq!(f.capacity().len(), 2);
    assert_eq!(
        f.stop(&lease(1)),
        StopAck::Stopped {
            read_back: Observed::Absent
        }
    );
    assert_eq!(f.booked().len(), 1);
}

/// Asserts that a scripted Unknown start answers Unknown and lands the booking only
/// when scripted to, and is spent after one call.
#[test]
fn scripted_unknown_start() {
    let mut f = fake();
    f.script_unknown(Call::Start, false);
    assert_eq!(f.start(spec(1, 1000)), StartAck::Unknown);
    assert!(f.booked().is_empty());
    f.script_unknown(Call::Start, true);
    assert_eq!(f.start(spec(1, 1000)), StartAck::Unknown);
    assert_eq!(f.booked().len(), 1);
    assert_eq!(
        f.start(spec(1, 1000)),
        StartAck::Started { host: host("h1") }
    );
}

/// Asserts that a scripted Unknown stop frees the host only when scripted to, and that
/// a script for another call is left alone.
#[test]
fn scripted_unknown_stop_and_observe() {
    let mut f = fake();
    f.start(spec(1, 1000));
    f.script_unknown(Call::Observe, false);
    f.script_unknown(Call::Stop, false);
    assert_eq!(f.stop(&lease(1)), StopAck::Unknown);
    assert_eq!(f.booked().len(), 1);
    assert_eq!(f.observe(&lease(1)), Observed::Unknown);
    assert_eq!(f.observe(&lease(1)), Observed::Present { host: host("h1") });
    f.script_unknown(Call::Stop, true);
    assert_eq!(f.stop(&lease(1)), StopAck::Unknown);
    assert!(f.booked().is_empty());
    assert_eq!(f.observe(&lease(1)), Observed::Absent);
}

/// Asserts that each defect changes exactly the behaviour it names.
#[test]
fn defects_break_one_behaviour_each() {
    let mut f = fake().with_defect(Defect::FreesOnUnknownObserve);
    f.start(spec(1, 1000));
    f.script_unknown(Call::Observe, false);
    assert_eq!(f.observe(&lease(1)), Observed::Unknown);
    assert!(f.booked().is_empty());

    let mut f = fake().with_defect(Defect::DoubleBooks);
    f.start(spec(1, 1000));
    f.start(spec(2, 1000));
    assert_eq!(f.booked().len(), 1);

    let mut f = fake().with_defect(Defect::RestartBooksAgain);
    f.start(spec(1, 1000));
    f.start(spec(1, 1000));
    assert_eq!(f.booked().len(), 2);

    let mut f = fake().with_defect(Defect::StopKeepsBooking);
    f.start(spec(1, 1000));
    assert_eq!(
        f.stop(&lease(1)),
        StopAck::Stopped {
            read_back: Observed::Absent
        }
    );
    assert_eq!(f.booked().len(), 1);
}

/// Asserts that a start of a stopped lease is refused with `LeaseEnded` and books
/// nothing, also for a lease stopped before its first start and for a stop that
/// answered Unknown but landed, and that an Unknown stop that did not land ends nothing.
#[test]
fn a_stopped_lease_never_starts_again() {
    let mut f = fake();
    f.start(spec(1, 1000));
    f.stop(&lease(1));
    let ended = StartAck::Refused(RefusalCode::LeaseEnded);
    assert_eq!(f.start(spec(1, 1000)), ended);
    assert!(f.booked().is_empty());
    assert_eq!(f.observe(&lease(1)), Observed::Absent);
    f.stop(&lease(2));
    assert_eq!(f.start(spec(2, 1000)), ended);

    f.start(spec(3, 1000));
    f.script_unknown(Call::Stop, false);
    assert_eq!(f.stop(&lease(3)), StopAck::Unknown);
    assert_eq!(
        f.start(spec(3, 1000)),
        StartAck::Started { host: host("h1") }
    );
    f.script_unknown(Call::Stop, true);
    f.stop(&lease(3));
    assert_eq!(f.start(spec(3, 1000)), ended);
    assert!(f.booked().is_empty());
}

/// Asserts that the resurrecting defect books a stopped lease again.
#[test]
fn the_resurrecting_defect_books_a_stopped_lease() {
    let mut f = fake().with_defect(Defect::ResurrectsStoppedLease);
    f.start(spec(1, 1000));
    f.stop(&lease(1));
    assert_eq!(
        f.start(spec(1, 1000)),
        StartAck::Started { host: host("h1") }
    );
    assert_eq!(f.booked().len(), 1);
}
