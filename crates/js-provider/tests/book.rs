//! The booking table the static pool and the registry of self-registered hosts share.

use std::collections::BTreeMap;
use std::time::Duration;

use js_provider::{HostBook, SeatError};
use js_provider::{Observed, RefusalCode, StartAck, StartSpec};
use js_types::{Admission, ClassId, HostId, LeaseId, Request, ResourceClass, Resources, Shape};

fn host(name: &str) -> HostId {
    HostId::new(name).unwrap()
}

fn shape(cpu_millis: u64) -> Shape {
    Shape::new(
        Resources::new(cpu_millis, 1, 0, None).unwrap(),
        BTreeMap::new(),
    )
}

fn class() -> ResourceClass {
    ResourceClass::new(ClassId::new("box").unwrap(), Admission::BookAll, 0)
}

fn spec(seq: u64) -> StartSpec {
    let request = Request {
        class: class().id().clone(),
        resources: Resources::new(1000, 1, 0, None).unwrap(),
        capabilities: BTreeMap::new(),
        max_wall: Duration::from_secs(60),
        labels: BTreeMap::new(),
        validation_run: None,
        probe_host: None,
    };
    StartSpec::new(LeaseId::new(1, seq), request, &[class()]).unwrap()
}

fn book() -> HostBook {
    let mut book = HostBook::new();
    book.insert_host(host("a"), shape(2000));
    book.insert_host(host("b"), shape(2000));
    book
}

/// Asserts that a host the predicate rejects is skipped for the next, that when every
/// fitting host is rejected the answer is NoCapacity (the host fits; it is not
/// available) and not NoFit, and that a rejected host does not lose its place once it is
/// accepted. A defect that let the predicate hide a fit would answer NoFit and tell a
/// caller to give up on a request that a later host could serve.
#[test]
fn the_predicate_chooses_among_fitting_free_hosts() {
    let mut book = book();
    let not_a = |id: &HostId, _: &Shape| id.as_str() != "a";
    let started = book.start_where(&spec(1), None, &not_a);
    assert_eq!(started, StartAck::Started { host: host("b") });
    let none = |_: &HostId, _: &Shape| false;
    let refused = book.start_where(&spec(2), None, &none);
    assert_eq!(refused, StartAck::Refused(RefusalCode::NoCapacity));
    let any = |_: &HostId, _: &Shape| true;
    assert_eq!(
        book.start_where(&spec(2), None, &any),
        StartAck::Started { host: host("a") }
    );
}

/// Asserts the order of answers of a start: a stopped lease is LeaseEnded even with a
/// gate, a held lease is Started again even with a gate, and a gate refuses before any
/// host is looked at.
#[test]
fn gate_comes_after_ended_and_held() {
    let mut book = book();
    let any = |_: &HostId, _: &Shape| true;
    let gate = Some(RefusalCode::UnsupportedClass);
    assert_eq!(
        book.start_where(&spec(1), gate, &any),
        StartAck::Refused(RefusalCode::UnsupportedClass)
    );
    assert!(book.booked().is_empty());
    book.start_where(&spec(1), None, &any);
    assert_eq!(
        book.start_where(&spec(1), gate, &any),
        StartAck::Started { host: host("a") }
    );
    book.stop(&LeaseId::new(1, 1));
    assert_eq!(
        book.start_where(&spec(1), gate, &any),
        StartAck::Refused(RefusalCode::LeaseEnded)
    );
}

/// Asserts that listing a host again changes neither its shape nor its booking, so a
/// caller that lists a host it already holds cannot free or resize a booked host.
#[test]
fn listing_a_listed_host_keeps_shape_and_booking() {
    let mut book = book();
    let any = |_: &HostId, _: &Shape| true;
    book.start_where(&spec(1), None, &any);
    book.insert_host(host("a"), shape(9000));
    let listed = book.hosts();
    assert_eq!(listed[0].shape, shape(2000));
    assert_eq!(listed[0].leased_to, Some(LeaseId::new(1, 1)));
    assert!(book.contains(&host("a")));
    assert!(!book.contains(&host("zz")));
}

/// Asserts that stop reads back Absent and seat of an unlisted host names the host.
#[test]
fn stop_reads_absent_and_seat_refuses_unlisted() {
    let mut book = book();
    assert_eq!(book.stop(&LeaseId::new(1, 5)), Observed::Absent);
    assert_eq!(
        book.seat(&host("zz"), LeaseId::new(1, 6)),
        Err(SeatError::Unlisted(host("zz")))
    );
}

/// Asserts that removing a free host unlists it and answers true, that a booked host is
/// kept with its booking and answers false, and that an unlisted host answers false. A
/// defect that removed a booked host would drop a lease's booking with its host.
#[test]
fn remove_free_unlists_only_a_free_host() {
    let mut book = book();
    book.start_where(&spec(1), None, &|_, _| true);
    assert!(!book.remove_free(&host("a")));
    assert_eq!(book.booked().len(), 1);
    assert!(book.contains(&host("a")));
    assert!(book.remove_free(&host("b")));
    assert!(!book.contains(&host("b")));
    assert!(!book.remove_free(&host("b")));
}

/// Asserts the class gate: a listed `Whole` class is leasable, a listed `Book` class is
/// still `UnsupportedClass`, and an unlisted class is refused. A gate that only knew
/// `BookAll` would refuse the whole-node class.
#[test]
fn class_gate_leases_whole_classes_only() {
    let make = |admission| ResourceClass::new(ClassId::new("w").unwrap(), admission, 0);
    let spec_of = |class: &ResourceClass| {
        let request = Request {
            class: class.id().clone(),
            resources: Resources::new(1000, 1, 0, None).unwrap(),
            capabilities: BTreeMap::new(),
            max_wall: Duration::from_secs(60),
            labels: BTreeMap::new(),
            validation_run: None,
            probe_host: None,
        };
        StartSpec::new(LeaseId::new(1, 1), request, std::slice::from_ref(class)).unwrap()
    };
    let whole = make(Admission::Whole);
    assert_eq!(
        HostBook::class_gate(std::slice::from_ref(&whole), &spec_of(&whole)),
        None
    );
    let booked = make(Admission::Book(Default::default()));
    assert_eq!(
        HostBook::class_gate(std::slice::from_ref(&booked), &spec_of(&booked)),
        Some(RefusalCode::UnsupportedClass)
    );
    assert_eq!(
        HostBook::class_gate(&[], &spec_of(&whole)),
        Some(RefusalCode::UnsupportedClass)
    );
}

/// `spec` `seq`, a probe pinned to `pin`.
fn probe_spec(seq: u64, pin: &str) -> StartSpec {
    let mut request = spec(seq).request().clone();
    request.probe_host = Some(host(pin));
    StartSpec::new(LeaseId::new(1, seq), request, &[class()]).unwrap()
}

/// Asserts that booking by the first eligible host places no probe (it has no rule to
/// judge one by) and books nothing for it, while booking by the caller's choice shows the
/// caller the probe's host alone, even when an earlier host by id is free; with that host
/// full the answer is NoCapacity, never another host. It catches a probe booked on a
/// host it does not name, which would put work on a host its operator kept apart.
#[test]
fn a_probe_is_booked_only_by_choice_and_only_on_its_host() {
    let mut book = book();
    let any = |_: &HostId, _: &Shape| true;
    let refused = book.start_where(&probe_spec(1, "b"), None, &any);
    assert_eq!(refused, StartAck::Refused(RefusalCode::ProbeUnsupported));
    assert_eq!(book.read(&LeaseId::new(1, 1)), Observed::Absent);

    let shown = std::cell::RefCell::new(Vec::new());
    let first = |open: &[js_provider::Candidate]| {
        shown.replace(open.iter().map(|c| c.host.clone()).collect());
        Some(0)
    };
    let started = book.start_by(&probe_spec(2, "b"), None, &any, &first);
    assert_eq!(started, StartAck::Started { host: host("b") });
    assert_eq!(shown.take(), [host("b")]);

    // `b` (2000 millicores) takes one more 1000-millicore lease, then is full.
    book.start_by(
        &spec(3),
        None,
        &|id: &HostId, _: &Shape| id.as_str() == "b",
        &first,
    );
    let full = book.start_by(&probe_spec(4, "b"), None, &any, &first);
    assert_eq!(full, StartAck::Refused(RefusalCode::NoCapacity));
    assert_eq!(book.read(&LeaseId::new(1, 4)), Observed::Absent);
    let nowhere = book.start_by(&probe_spec(5, "c"), None, &any, &first);
    assert_eq!(nowhere, StartAck::Refused(RefusalCode::NoFit));
}
