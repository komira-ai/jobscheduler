//! A book's CPU slack ([`HostBook::set_cpu_slack`]): leases that share a host may book
//! together more CPU than its shape has, by the slack, and no more of anything else; and
//! the full listing a chooser can read beside the candidates ([`HostBook::start_over`]).

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::time::Duration;

use js_provider::{Candidate, HostBook, SeatError};
use js_provider::{RefusalCode, StartAck, StartSpec};
use js_types::{Admission, ClassId, HostId, LeaseId, Request, ResourceClass, Resources, Shape};

fn host(name: &str) -> HostId {
    HostId::new(name).unwrap()
}

/// A host of 10 cores and 16 bytes.
fn shape() -> Shape {
    Shape::new(
        Resources::new(10_000, 16, 0, None).unwrap(),
        BTreeMap::new(),
    )
}

fn sharing() -> ResourceClass {
    let id = ClassId::new("share").unwrap();
    ResourceClass::new(id, Admission::Book(Default::default()), 0)
}

fn whole() -> ResourceClass {
    ResourceClass::new(ClassId::new("box").unwrap(), Admission::BookAll, 0)
}

fn spec(class: &ResourceClass, seq: u64, cpu_millis: u64, memory: u64) -> StartSpec {
    let request = Request {
        class: class.id().clone(),
        resources: Resources::new(cpu_millis, memory, 0, None).unwrap(),
        capabilities: BTreeMap::new(),
        max_wall: Duration::from_secs(60),
        labels: BTreeMap::new(),
        validation_run: None,
        probe_host: None,
    };
    StartSpec::new(LeaseId::new(1, seq), request, std::slice::from_ref(class)).unwrap()
}

/// A book of host `a`, booked with ten one-core leases of one byte each (sequences 1 to
/// 10), with `slack` set before.
fn full(slack: u32) -> HostBook {
    let mut book = HostBook::new();
    book.set_cpu_slack(slack);
    book.insert_host(host("a"), shape());
    for seq in 1..=10 {
        let started = book.start_where(&spec(&sharing(), seq, 1_000, 1), None, ANY);
        assert_eq!(started, StartAck::Started { host: host("a") });
    }
    book
}

const ANY: &dyn Fn(&HostId, &Shape) -> bool = &|_, _| true;

const NO_CAPACITY: StartAck = StartAck::Refused(RefusalCode::NoCapacity);

/// Catches: the slack not read (a host whose cores are booked refuses one more core), read
/// as a share of the start rather than of the host (1.2 cores pass), or applied to memory.
/// At 100 thousandths a 10-core host booked at 10 cores takes one more core and not 1.2;
/// at 0 it takes none; memory past the shape is refused whatever the slack.
#[test]
fn a_slack_lets_shared_cpu_book_that_far_over_the_cores_and_memory_not_at_all() {
    let start = |book: &mut HostBook, seq, cpu, memory| {
        book.start_by(&spec(&sharing(), seq, cpu, memory), None, ANY, &|_| Some(0))
    };
    let mut book = full(100);
    assert_eq!(start(&mut book, 11, 1_200, 1), NO_CAPACITY);
    assert_eq!(
        start(&mut book, 12, 1_000, 1),
        StartAck::Started { host: host("a") }
    );
    assert_eq!(start(&mut book, 13, 1, 1), NO_CAPACITY);
    let mut book = full(100);
    assert_eq!(start(&mut book, 11, 1, 7), NO_CAPACITY);
    let mut book = full(0);
    assert_eq!(start(&mut book, 11, 1, 1), NO_CAPACITY);
    assert_eq!(
        full(0).start_where(&spec(&sharing(), 11, 1, 1), None, ANY),
        NO_CAPACITY
    );
    let mut book = full(100);
    let first_fit = book.start_where(&spec(&sharing(), 11, 1_000, 1), None, ANY);
    assert_eq!(first_fit, StartAck::Started { host: host("a") });
}

/// Catches: a slack that lets a whole-host lease onto a host that carries leases, or a
/// lease that shares onto one held whole, and a seat that ignores the slack (a booking put
/// back after a restart refused though a start took it).
#[test]
fn a_slack_changes_nothing_for_a_whole_host_lease_and_holds_for_a_seat() {
    let mut book = full(100);
    let whole_start = book.start_where(&spec(&whole(), 11, 1, 1), None, ANY);
    assert_eq!(whole_start, NO_CAPACITY);
    let one_core = Resources::new(1_000, 1, 0, None).unwrap();
    assert_eq!(
        book.seat_as(&host("a"), LeaseId::new(1, 12), Some(one_core)),
        Ok(())
    );
    let more = Resources::new(1, 1, 0, None).unwrap();
    let refused = book.seat_as(&host("a"), LeaseId::new(1, 13), Some(more));
    let taken = SeatError::Taken {
        host: host("a"),
        holder: LeaseId::new(1, 1),
    };
    assert_eq!(refused, Err(taken));
    let mut held = HostBook::new();
    held.set_cpu_slack(1_000);
    held.insert_host(host("b"), shape());
    let owner = held.start_where(&spec(&whole(), 1, 1, 1), None, ANY);
    assert_eq!(owner, StartAck::Started { host: host("b") });
    let beside = held.start_where(&spec(&sharing(), 2, 1, 1), None, ANY);
    assert_eq!(beside, NO_CAPACITY);
}

/// Catches: a listing that leaves out a host the start cannot use (a full one, or one
/// that is not eligible) or the leases on a host, and a `start_by` whose chooser is not
/// the one `start_over` asks. The chooser of `start_over` is shown `b` alone as a
/// candidate and reads both hosts with their leases from the listing; `candidates` lists
/// the same.
#[test]
fn a_chooser_can_read_every_listed_host_beside_the_candidates() {
    let mut book = full(0);
    book.insert_host(host("b"), shape());
    let seen = RefCell::new(Vec::new());
    let choose = |open: &[Candidate], every: &dyn Fn() -> Vec<Candidate>| {
        let named = |list: &[Candidate]| {
            let shown = list
                .iter()
                .map(|c| (c.host.as_str().to_owned(), c.leases.len()));
            shown.collect::<Vec<_>>()
        };
        seen.borrow_mut().push((named(open), named(&every())));
        Some(0)
    };
    let started = book.start_over(&spec(&sharing(), 11, 1_000, 1), None, ANY, &choose);
    assert_eq!(started, StartAck::Started { host: host("b") });
    let open = vec![("b".to_owned(), 0)];
    let every = vec![("a".to_owned(), 10), ("b".to_owned(), 0)];
    assert_eq!(seen.into_inner(), [(open, every)]);
    let listed = book.candidates();
    let names: Vec<_> = listed
        .iter()
        .map(|c| (c.host.as_str(), c.leases.len()))
        .collect();
    assert_eq!(names, [("a", 10), ("b", 1)]);
    assert_eq!(listed[1].shape, shape());
}
