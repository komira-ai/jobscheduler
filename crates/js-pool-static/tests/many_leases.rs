//! A host books many leases: a lease that shares a host books its own resources, and a
//! whole-host lease books the host. Starts that book a whole host behave as they always did.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::time::Duration;

use js_pool_static::{Candidate, HostBook, SeatError};
use js_provider::{Observed, RefusalCode, StartAck, StartSpec};
use js_types::{Admission, ClassId, HostId, LeaseId, Request, ResourceClass, Resources, Shape};

fn host(name: &str) -> HostId {
    HostId::new(name).unwrap()
}

fn shape() -> Shape {
    Shape::new(Resources::new(2000, 4, 0, None).unwrap(), BTreeMap::new())
}

fn sharing() -> ResourceClass {
    ResourceClass::new(
        ClassId::new("share").unwrap(),
        Admission::Book(Default::default()),
        0,
    )
}

fn whole() -> ResourceClass {
    ResourceClass::new(ClassId::new("box").unwrap(), Admission::BookAll, 0)
}

fn spec(class: &ResourceClass, seq: u64, cpu_millis: u64) -> StartSpec {
    spec_of(class, seq, Resources::new(cpu_millis, 1, 0, None).unwrap())
}

fn spec_of(class: &ResourceClass, seq: u64, resources: Resources) -> StartSpec {
    let request = Request {
        class: class.id().clone(),
        resources,
        capabilities: BTreeMap::new(),
        max_wall: Duration::from_secs(60),
        labels: BTreeMap::new(),
        validation_run: None,
    };
    StartSpec::new(LeaseId::new(1, seq), request, std::slice::from_ref(class)).unwrap()
}

fn one_host() -> HostBook {
    let mut book = HostBook::new();
    book.insert_host(host("a"), shape());
    book
}

const ANY: &dyn Fn(&HostId, &Shape) -> bool = &|_, _| true;

/// Asserts that a second lease starts on the host that already carries one when the host
/// has room for it, that both read `Present` there, and that `leases` lists both in the
/// order they were booked.
#[test]
fn a_second_lease_starts_on_a_host_with_room() {
    let mut book = one_host();
    let class = sharing();
    let first = book.start_where(&spec(&class, 1, 1000), None, ANY);
    let second = book.start_where(&spec(&class, 2, 1000), None, ANY);
    assert_eq!(first, StartAck::Started { host: host("a") });
    assert_eq!(second, StartAck::Started { host: host("a") });
    assert_eq!(
        book.read(&LeaseId::new(1, 2)),
        Observed::Present { host: host("a") }
    );
    let held = vec![LeaseId::new(1, 1), LeaseId::new(1, 2)];
    assert_eq!(book.leases(), BTreeMap::from([(host("a"), held)]));
}

/// Asserts that a lease that does not fit in what the host has left is refused for
/// capacity, not for fit, and that stopping one lease gives its room back to the next.
#[test]
fn a_full_host_refuses_a_third_lease_until_one_stops() {
    let mut book = one_host();
    let class = sharing();
    book.start_where(&spec(&class, 1, 1000), None, ANY);
    book.start_where(&spec(&class, 2, 1000), None, ANY);
    assert_eq!(
        book.start_where(&spec(&class, 3, 1000), None, ANY),
        StartAck::Refused(RefusalCode::NoCapacity)
    );
    assert_eq!(book.stop(&LeaseId::new(1, 1)), Observed::Absent);
    assert_eq!(
        book.start_where(&spec(&class, 3, 1000), None, ANY),
        StartAck::Started { host: host("a") }
    );
}

/// Asserts that a whole-host lease takes only a host with no lease, and that a lease
/// that shares takes no host a whole-host lease holds.
#[test]
fn a_whole_host_lease_and_a_sharing_lease_exclude_each_other() {
    let mut book = one_host();
    let (share, all) = (sharing(), whole());
    book.start_where(&spec(&share, 1, 500), None, ANY);
    assert_eq!(
        book.start_where(&spec(&all, 2, 1000), None, ANY),
        StartAck::Refused(RefusalCode::NoCapacity)
    );
    book.stop(&LeaseId::new(1, 1));
    assert_eq!(
        book.start_where(&spec(&all, 3, 1000), None, ANY),
        StartAck::Started { host: host("a") }
    );
    assert_eq!(
        book.start_where(&spec(&share, 4, 500), None, ANY),
        StartAck::Refused(RefusalCode::NoCapacity)
    );
}

/// Asserts that a host that carries a lease is not unlisted, however many it carries,
/// and is unlisted once the last lease stops.
#[test]
fn a_host_with_any_lease_is_not_removed() {
    let mut book = one_host();
    let class = sharing();
    book.start_where(&spec(&class, 1, 500), None, ANY);
    book.start_where(&spec(&class, 2, 500), None, ANY);
    book.stop(&LeaseId::new(1, 1));
    assert!(!book.remove_free(&host("a")));
    book.stop(&LeaseId::new(1, 2));
    assert!(book.remove_free(&host("a")));
}

/// Asserts that the single-lease views do not change: a whole-host start books one
/// lease, a second whole-host start is refused for capacity, `booked` and the capacity
/// list name that lease, and `leases` lists it alone.
#[test]
fn whole_host_bookings_read_as_they_always_did() {
    let mut book = one_host();
    let class = whole();
    assert_eq!(
        book.start_where(&spec(&class, 1, 1000), None, ANY),
        StartAck::Started { host: host("a") }
    );
    assert_eq!(
        book.start_where(&spec(&class, 2, 1000), None, ANY),
        StartAck::Refused(RefusalCode::NoCapacity)
    );
    let lease = LeaseId::new(1, 1);
    assert_eq!(book.booked(), BTreeMap::from([(host("a"), lease)]));
    assert_eq!(book.hosts()[0].leased_to, Some(lease));
    assert_eq!(book.leases(), BTreeMap::from([(host("a"), vec![lease])]));
}

/// Asserts that with two leases on a host, `booked` and the capacity list name the
/// lease booked first, which is the lease a one-lease reader has always seen.
#[test]
fn the_single_lease_views_name_the_first_lease() {
    let mut book = one_host();
    let class = sharing();
    book.start_where(&spec(&class, 7, 500), None, ANY);
    book.start_where(&spec(&class, 3, 500), None, ANY);
    let first = LeaseId::new(1, 7);
    assert_eq!(book.booked(), BTreeMap::from([(host("a"), first)]));
    assert_eq!(book.hosts()[0].leased_to, Some(first));
}

fn book_of(resources: Resources) -> HostBook {
    let mut book = HostBook::new();
    book.insert_host(host("a"), Shape::new(resources, BTreeMap::new()));
    book
}

fn lease(seq: u64) -> LeaseId {
    LeaseId::new(1, seq)
}

/// Asserts that a sharing request of all zeros is refused beside a whole-host lease:
/// the whole lease excludes it by kind, not because the sums happen to overflow the shape.
#[test]
fn a_zero_request_is_refused_beside_a_whole_host_lease() {
    let mut book = one_host();
    let (share, all) = (sharing(), whole());
    book.start_where(&spec(&all, 1, 1000), None, ANY);
    let zero = Resources::new(0, 0, 0, None).unwrap();
    assert_eq!(
        book.start_where(&spec_of(&share, 2, zero), None, ANY),
        StartAck::Refused(RefusalCode::NoCapacity)
    );
}

/// Guard (the one-lease code agrees on the refusal): leases that add up to exactly the
/// shape in every dimension fit, and one more unit does not.
#[test]
fn leases_that_add_up_to_exactly_the_shape_fit() {
    let mut book = one_host();
    let class = sharing();
    let half = || Resources::new(1000, 2, 0, None).unwrap();
    book.start_where(&spec_of(&class, 1, half()), None, ANY);
    assert_eq!(
        book.start_where(&spec_of(&class, 2, half()), None, ANY),
        StartAck::Started { host: host("a") }
    );
    let one = Resources::new(1, 0, 0, None).unwrap();
    assert_eq!(
        book.start_where(&spec_of(&class, 3, one), None, ANY),
        StartAck::Refused(RefusalCode::NoCapacity)
    );
}

/// Guard: stopping the middle of three leases keeps the other two, in booking order.
#[test]
fn stopping_the_middle_lease_keeps_the_others_in_order() {
    let mut book = one_host();
    let class = sharing();
    for seq in [1, 2, 3] {
        book.start_where(&spec(&class, seq, 500), None, ANY);
    }
    book.stop(&lease(2));
    assert_eq!(
        book.leases(),
        BTreeMap::from([(host("a"), vec![lease(1), lease(3)])])
    );
    assert_eq!(book.read(&lease(2)), Observed::Absent);
}

/// Guard: two leases that each want the host's only GPU do not share it.
#[test]
fn two_leases_cannot_share_one_gpu() {
    let gpu = || Resources::new(500, 1, 1, Some("a100".into())).unwrap();
    let mut book = book_of(Resources::new(2000, 4, 1, Some("a100".into())).unwrap());
    let class = sharing();
    assert_eq!(
        book.start_where(&spec_of(&class, 1, gpu()), None, ANY),
        StartAck::Started { host: host("a") }
    );
    assert_eq!(
        book.start_where(&spec_of(&class, 2, gpu()), None, ANY),
        StartAck::Refused(RefusalCode::NoCapacity)
    );
}

/// Guard: memory binds on its own, with cpu to spare.
#[test]
fn memory_alone_can_refuse_a_lease() {
    let mut book = one_host();
    let class = sharing();
    let heavy = |gib| Resources::new(100, gib, 0, None).unwrap();
    book.start_where(&spec_of(&class, 1, heavy(3)), None, ANY);
    assert_eq!(
        book.start_where(&spec_of(&class, 2, heavy(2)), None, ANY),
        StartAck::Refused(RefusalCode::NoCapacity)
    );
    assert_eq!(
        book.start_where(&spec_of(&class, 3, heavy(1)), None, ANY),
        StartAck::Started { host: host("a") }
    );
}

/// Asserts that a sum that overflows is refused, not wrapped into a fit.
#[test]
fn an_overflowing_sum_is_refused() {
    let mut book = book_of(Resources::new(u64::MAX, 4, 0, None).unwrap());
    let class = sharing();
    let big = Resources::new(u64::MAX - 1, 1, 0, None).unwrap();
    book.start_where(&spec_of(&class, 1, big), None, ANY);
    let small = Resources::new(2, 1, 0, None).unwrap();
    assert_eq!(
        book.start_where(&spec_of(&class, 2, small), None, ANY),
        StartAck::Refused(RefusalCode::NoCapacity)
    );
}

/// Guard: a lease stopped on a host that carries several never starts again.
#[test]
fn a_stopped_lease_never_restarts_on_a_many_lease_host() {
    let mut book = one_host();
    let class = sharing();
    book.start_where(&spec(&class, 1, 500), None, ANY);
    book.start_where(&spec(&class, 2, 500), None, ANY);
    book.stop(&lease(1));
    assert_eq!(
        book.start_where(&spec(&class, 1, 500), None, ANY),
        StartAck::Refused(RefusalCode::LeaseEnded)
    );
    assert_eq!(book.leases().get(&host("a")), Some(&vec![lease(2)]));
}

/// A chooser, as `start_by` takes it.
type Choose = dyn Fn(&[Candidate]) -> Option<usize>;

fn two_hosts() -> HostBook {
    let mut book = HostBook::new();
    book.insert_host(host("a"), shape());
    book.insert_host(host("b"), shape());
    book
}

/// Asserts that `start_by` shows its chooser every candidate in id order with the leases
/// already booked on each, and books the host the chooser picks. The chooser here picks the
/// last candidate, which first-fit never does: a chooser that is ignored books `a`.
#[test]
fn the_caller_picks_the_host_from_the_candidates() {
    let mut book = two_hosts();
    let class = sharing();
    let seen = RefCell::new(Vec::new());
    let last = |open: &[Candidate]| {
        let shown = open.iter().map(|c| (c.host.clone(), c.leases.len()));
        seen.borrow_mut().push(shown.collect::<Vec<_>>());
        open.len().checked_sub(1)
    };
    let first = book.start_by(&spec(&class, 1, 1000), None, ANY, &last);
    let second = book.start_by(&spec(&class, 2, 1000), None, ANY, &last);
    assert_eq!(first, StartAck::Started { host: host("b") });
    assert_eq!(second, StartAck::Started { host: host("b") });
    let shown = vec![
        vec![(host("a"), 0), (host("b"), 0)],
        vec![(host("a"), 0), (host("b"), 1)],
    ];
    assert_eq!(seen.into_inner(), shown);
}

/// Asserts that a chooser that picks none, or an index past the candidates, books nothing
/// and the start is `NoCapacity` (the hosts fit), and that a chooser is not asked when no
/// host has room. A chooser that is ignored starts the lease.
#[test]
fn a_chooser_that_declines_leaves_the_start_at_no_capacity() {
    let mut book = one_host();
    let class = sharing();
    let none = |_: &[Candidate]| None;
    let past = |open: &[Candidate]| Some(open.len());
    let chooses: [&Choose; 2] = [&none, &past];
    for (seq, choose) in (1..).zip(chooses) {
        assert_eq!(
            book.start_by(&spec(&class, seq, 500), None, ANY, choose),
            StartAck::Refused(RefusalCode::NoCapacity)
        );
    }
    assert!(book.leases().is_empty());
    let count = RefCell::new(Vec::new());
    let count_them = |open: &[Candidate]| {
        count.borrow_mut().push(open.len());
        None
    };
    book.start_where(&spec(&class, 3, 1500), None, ANY);
    book.start_by(&spec(&class, 4, 1000), None, ANY, &count_them);
    assert_eq!(count.into_inner(), Vec::<usize>::new());
}

/// Asserts the gate of a provider that lets leases share: a listed class passes whether it
/// books the whole host or part of it, and a class the provider does not list is refused
/// `UnsupportedClass`. The whole-host gate still refuses the class that shares.
#[test]
fn the_listed_gate_admits_a_class_that_shares() {
    let (share, all) = (sharing(), whole());
    let classes = [share.clone(), all.clone()];
    assert_eq!(HostBook::listed_gate(&classes, &spec(&share, 1, 500)), None);
    assert_eq!(HostBook::listed_gate(&classes, &spec(&all, 2, 500)), None);
    assert_eq!(
        HostBook::listed_gate(std::slice::from_ref(&all), &spec(&share, 3, 500)),
        Some(RefusalCode::UnsupportedClass)
    );
    assert_eq!(
        HostBook::class_gate(&classes, &spec(&share, 4, 500)),
        Some(RefusalCode::UnsupportedClass)
    );
}

/// Asserts that a lease that shares is put back with its own resources: two such leases
/// are seated on one host, the host then has room for what is left and none for more, and
/// stopping one gives its room back. Seating books the whole shape instead, so the second
/// seat is refused and the host is full.
#[test]
fn a_restored_sharing_lease_keeps_its_resources() {
    let mut book = one_host();
    let class = sharing();
    let half = || Some(Resources::new(1000, 1, 0, None).unwrap());
    assert_eq!(book.seat_as(&host("a"), lease(1), half()), Ok(()));
    assert_eq!(book.seat_as(&host("a"), lease(2), half()), Ok(()));
    assert_eq!(
        book.start_where(&spec(&class, 3, 500), None, ANY),
        StartAck::Refused(RefusalCode::NoCapacity)
    );
    book.stop(&lease(1));
    assert_eq!(
        book.start_where(&spec(&class, 4, 1000), None, ANY),
        StartAck::Started { host: host("a") }
    );
    assert_eq!(
        book.leases().get(&host("a")),
        Some(&vec![lease(2), lease(4)])
    );
}

/// Asserts the refusals of seating with resources: a lease too big for an empty host is
/// `NoRoom`; one that shares beside a whole-host lease, or a whole-host lease beside one
/// that shares, or a lease that does not fit beside the others, is `Taken` and names the
/// first lease there; a lease already seated elsewhere is `Elsewhere`.
#[test]
fn seating_with_resources_refuses_what_does_not_fit() {
    let mut book = two_hosts();
    let big = Some(Resources::new(3000, 1, 0, None).unwrap());
    let one = || Some(Resources::new(1000, 1, 0, None).unwrap());
    assert_eq!(
        book.seat_as(&host("a"), lease(1), big),
        Err(SeatError::NoRoom(host("a")))
    );
    assert_eq!(book.seat(&host("a"), lease(2)), Ok(()));
    let taken = SeatError::Taken {
        host: host("a"),
        holder: lease(2),
    };
    assert_eq!(book.seat_as(&host("a"), lease(3), one()), Err(taken));
    assert_eq!(book.seat_as(&host("b"), lease(4), one()), Ok(()));
    let beside = SeatError::Taken {
        host: host("b"),
        holder: lease(4),
    };
    assert_eq!(
        book.seat_as(&host("b"), lease(5), None),
        Err(beside.clone())
    );
    let most = Some(Resources::new(1500, 1, 0, None).unwrap());
    assert_eq!(book.seat_as(&host("b"), lease(6), most), Err(beside));
    let elsewhere = SeatError::Elsewhere {
        lease: lease(4),
        host: host("b"),
    };
    assert_eq!(book.seat_as(&host("a"), lease(4), one()), Err(elsewhere));
    assert_eq!(book.seat_as(&host("b"), lease(4), one()), Ok(()));
}

/// Asserts that `start_by` answers as `start_where` does before it looks at any host: a
/// stopped lease is `LeaseEnded`, a lease that holds a host is `Started` there again
/// without asking the chooser, and a gate refusal is passed on. A chooser asked in these
/// cases would book a second time or ignore the gate.
#[test]
fn start_by_settles_a_start_before_it_asks_the_chooser() {
    let mut book = one_host();
    let class = sharing();
    let asked = RefCell::new(0);
    let first = |open: &[Candidate]| {
        *asked.borrow_mut() += 1;
        (!open.is_empty()).then_some(0)
    };
    let started = book.start_by(&spec(&class, 1, 500), None, ANY, &first);
    assert_eq!(started, StartAck::Started { host: host("a") });
    let again = book.start_by(&spec(&class, 1, 500), None, ANY, &first);
    assert_eq!(again, StartAck::Started { host: host("a") });
    let gate = Some(RefusalCode::UnsupportedClass);
    let gated = book.start_by(&spec(&class, 2, 500), gate, ANY, &first);
    assert_eq!(gated, StartAck::Refused(RefusalCode::UnsupportedClass));
    book.stop(&lease(1));
    let ended = book.start_by(&spec(&class, 1, 500), None, ANY, &first);
    assert_eq!(ended, StartAck::Refused(RefusalCode::LeaseEnded));
    assert_eq!(asked.into_inner(), 1);
}

/// Asserts what `held` reports a lease books: its own resources for a lease that shares
/// the host, the host's whole shape for one that books all of it, and nothing for a lease
/// that is not booked or was stopped. It catches a booking read as another lease's.
#[test]
fn held_reports_what_each_lease_books() {
    let mut book = one_host();
    book.insert_host(host("b"), shape());
    let started = book.start_where(&spec(&sharing(), 1, 500), None, ANY);
    assert_eq!(started, StartAck::Started { host: host("a") });
    let started = book.start_where(&spec(&whole(), 2, 500), None, ANY);
    assert_eq!(started, StartAck::Started { host: host("b") });
    let shared = book.held(&LeaseId::new(1, 1)).unwrap();
    assert_eq!(shared.resources, Resources::new(500, 1, 0, None).unwrap());
    assert!(!shared.whole);
    let all = book.held(&LeaseId::new(1, 2)).unwrap();
    assert_eq!((&all.resources, all.whole), (shape().resources(), true));
    assert_eq!(book.held(&LeaseId::new(1, 3)), None);
    book.stop(&LeaseId::new(1, 1));
    assert_eq!(book.held(&LeaseId::new(1, 1)), None);
}
