//! The booking table the static pool and the registry of self-registered hosts share.

use std::collections::BTreeMap;
use std::time::Duration;

use js_pool_static::{HostBook, SeatError};
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
