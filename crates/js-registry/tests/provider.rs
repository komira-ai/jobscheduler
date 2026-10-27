//! What the registered provider books, observes and stops, as the lease table sees it.

mod support;

use js_agent_wire::{Phase, refusal};
use js_pool_static::SeatError;
use js_provider::{Flavour, Observed, Provider, RefusalCode, StartAck, StopAck};
use js_registry::{HostState, Refusal};
use js_types::{Admission, ClassId, ResourceClass};
use support::*;

fn stopped(ack: StopAck) -> Observed {
    match ack {
        StopAck::Stopped { read_back } => read_back,
        StopAck::Unknown => panic!("a registry always answers a stop"),
    }
}

/// Asserts that a lost host keeps its booking and the lease observes Unknown, never
/// Absent: the table frees a booking only on Absent, so a defect that reported Absent
/// would drop a lease because its host was silent. A heartbeat makes it Present again.
#[test]
fn lost_host_observes_unknown_keeps_booking() {
    let mut fx = fixture(4);
    let epoch = fx.join("a", "t");
    let ack = fx.provider.start(spec(1));
    assert_eq!(started_on(&ack), Some("a"));
    assert_eq!(
        fx.provider.observe(&lease(1)),
        Observed::Present { host: host("a") }
    );
    fx.pass_grace();
    assert_eq!(fx.registry.hosts()[0].state, HostState::Lost);
    let seen = fx.provider.observe(&lease(1));
    assert_eq!(seen, Observed::Unknown);
    assert!(!seen.frees_booking());
    assert_eq!(fx.provider.booked().get(&host("a")), Some(&lease(1)));
    assert_eq!(fx.provider.capacity()[0].leased_to, Some(lease(1)));
    fx.beat("a", epoch);
    assert_eq!(
        fx.provider.observe(&lease(1)),
        Observed::Present { host: host("a") }
    );
}

/// Asserts that a lost host is not booked for a new lease, whether or not another lease
/// holds the others, and that its next heartbeat makes it bookable again: a defect
/// that leased a dead host would hand a consumer a machine nobody can reach. A repeated
/// start of a lease the lost host already holds still answers Started.
#[test]
fn lost_host_not_bookable_until_next_heartbeat() {
    let mut fx = fixture(4);
    let epoch = fx.join("a", "t");
    fx.pass_grace();
    let refused = fx.provider.start(spec(1));
    assert_eq!(refused, StartAck::Refused(RefusalCode::NoCapacity));
    assert!(fx.provider.booked().is_empty());
    fx.beat("a", epoch);
    assert_eq!(started_on(&fx.provider.start(spec(1))), Some("a"));
    fx.pass_grace();
    assert_eq!(started_on(&fx.provider.start(spec(1))), Some("a"));
    assert_eq!(
        fx.provider.start(spec(2)),
        StartAck::Refused(RefusalCode::NoCapacity)
    );
}

/// Asserts that a start takes the first host by id that is live, serving and free, and
/// skips one that is booked or lost, so placement is deterministic and a lost host is
/// passed over for the next.
#[test]
fn start_books_the_first_live_free_host_by_id() {
    let mut fx = fixture(4);
    fx.join("c", "t");
    fx.join("a", "t");
    fx.join("b", "t");
    assert_eq!(started_on(&fx.provider.start(spec(1))), Some("a"));
    assert_eq!(started_on(&fx.provider.start(spec(2))), Some("b"));
    assert_eq!(started_on(&fx.provider.start(spec(3))), Some("c"));
    assert_eq!(
        fx.provider.start(spec(4)),
        StartAck::Refused(RefusalCode::NoCapacity)
    );
}

/// Asserts that a host announcing draining, quarantined, an unknown phase text, or any
/// other phase than serving takes no lease, in the hello or in a later heartbeat; that
/// the lease it already holds stays Present; and that serving again makes it bookable.
#[test]
fn draining_and_unknown_phase_take_no_lease() {
    let mut fx = fixture(4);
    let unknown: Phase = serde_json::from_value("from-a-newer-agent".into()).unwrap();
    assert_eq!(unknown, Phase::Quarantined);
    let mut first = hello("a");
    first.phase = unknown;
    let epoch = fx.hello(&first, &cred("t")).unwrap().epoch;
    assert_eq!(
        fx.provider.start(spec(1)),
        StartAck::Refused(RefusalCode::NoCapacity)
    );
    for phase in [
        Phase::Draining,
        Phase::Quarantined,
        Phase::Preparing,
        Phase::Restoring,
        Phase::MemoryPressure,
        Phase::Tainted,
    ] {
        let mut beat = fx.heartbeat("a", epoch);
        beat.phase = phase;
        fx.registry.heartbeat(&beat, &cred("t")).unwrap();
        assert_eq!(
            fx.provider.start(spec(1)),
            StartAck::Refused(RefusalCode::NoCapacity)
        );
    }
    fx.beat("a", epoch);
    assert_eq!(started_on(&fx.provider.start(spec(1))), Some("a"));
    let mut draining = fx.heartbeat("a", epoch);
    draining.phase = Phase::Draining;
    fx.registry.heartbeat(&draining, &cred("t")).unwrap();
    assert_eq!(
        fx.provider.observe(&lease(1)),
        Observed::Present { host: host("a") }
    );
    assert_eq!(
        fx.provider.start(spec(2)),
        StartAck::Refused(RefusalCode::NoCapacity)
    );
}

/// Asserts that a lease seated on a host the registry does not know observes Unknown
/// (the table keeps it), the host takes no other lease and cannot be heartbeat to, and
/// that the agent's hello binds it: the lease is then Present on it. Without the seat a
/// server restart would end every lease on a registered host before its agent returned.
#[test]
fn awaiting_seat_unknown_then_binds_on_hello() {
    let mut fx = fixture(4);
    fx.provider.seat(&host("a"), lease(1), None).unwrap();
    assert_eq!(fx.provider.observe(&lease(1)), Observed::Unknown);
    let awaiting = &fx.registry.hosts()[0];
    assert_eq!(
        (awaiting.state, awaiting.epoch, awaiting.lease),
        (HostState::Awaiting, 0, Some(lease(1)))
    );
    assert_eq!(
        fx.provider.start(spec(2)),
        StartAck::Refused(RefusalCode::NoCapacity)
    );
    assert_eq!(
        fx.registry
            .heartbeat(&heartbeat("a", 0), &cred("t"))
            .unwrap_err(),
        Refusal::UnknownHost
    );
    assert_eq!(fx.join("a", "t"), 1);
    assert_eq!(
        fx.provider.observe(&lease(1)),
        Observed::Present { host: host("a") }
    );
    assert_eq!(fx.registry.hosts()[0].state, HostState::Live);
    assert_eq!(
        fx.registry.hello(&hello("a"), &cred("other")).unwrap_err(),
        Refusal::HostBound
    );
}

/// Asserts that an awaiting host with no hello still must satisfy the shape on its
/// hello: a seat does not exempt a host from the pool's shape.
#[test]
fn awaiting_host_still_checked_at_hello() {
    let fx = fixture(4);
    fx.provider.seat(&host("a"), lease(1), None).unwrap();
    let mut small = hello("a");
    small.shape.memory_bytes = 1;
    assert_eq!(
        fx.registry.hello(&small, &cred("t")).unwrap_err().code(),
        refusal::SHAPE_NOT_ACCEPTED
    );
    assert_eq!(fx.registry.hosts()[0].state, HostState::Awaiting);
}

/// Asserts that a stop frees the booking and reads back Absent, also for a lost host, and
/// that a start of the stopped lease is then refused LeaseEnded: a stopped lease never
/// restarts, so a late duplicate start cannot book a second host.
#[test]
fn stop_frees_and_reads_absent_and_ended_lease_never_restarts() {
    let mut fx = fixture(4);
    fx.join("a", "t");
    fx.provider.start(spec(1));
    fx.pass_grace();
    assert_eq!(stopped(fx.provider.stop(&lease(1))), Observed::Absent);
    assert!(fx.provider.booked().is_empty());
    assert_eq!(fx.provider.observe(&lease(1)), Observed::Absent);
    assert_eq!(
        fx.provider.start(spec(1)),
        StartAck::Refused(RefusalCode::LeaseEnded)
    );
    assert_eq!(
        fx.provider.seat(&host("a"), lease(1), None),
        Err(SeatError::Ended(lease(1)))
    );
}

/// Asserts that a repeated start answers Started on the same host and books nothing more.
#[test]
fn start_is_idempotent() {
    let mut fx = fixture(4);
    fx.join("a", "t");
    fx.join("b", "t");
    let first = fx.provider.start(spec(1));
    assert_eq!(fx.provider.start(spec(1)), first);
    assert_eq!(fx.provider.booked().len(), 1);
}

/// Asserts the two refusals of a pool with nobody to book: a request the pool's shape
/// fits is NoCapacity when no host has joined yet (one could), and NoFit when the pool's
/// shape cannot fit it whoever joins.
#[test]
fn empty_pool_is_no_capacity_and_oversize_is_no_fit() {
    let mut fx = fixture(4);
    assert_eq!(
        fx.provider.start(spec(1)),
        StartAck::Refused(RefusalCode::NoCapacity)
    );
    let mut huge = request();
    huge.resources = js_types::Resources::new(4001, 1, 0, None).unwrap();
    let spec = js_provider::StartSpec::new(lease(2), huge, &[class()]).unwrap();
    assert_eq!(
        fx.provider.start(spec),
        StartAck::Refused(RefusalCode::NoFit)
    );
    fx.join("a", "t");
    let mut needs_linux = request();
    needs_linux.capabilities = map(&[("os", "linux")]);
    let spec = js_provider::StartSpec::new(lease(3), needs_linux, &[class()]).unwrap();
    assert_eq!(
        fx.provider.start(spec),
        StartAck::Refused(RefusalCode::NoFit)
    );
}

/// Asserts the provider's identity, and that a class the provider does not list or one
/// that does not book the whole host is refused UnsupportedClass and books nothing.
#[test]
fn identity_and_class_gate() {
    let mut fx = fixture(4);
    fx.join("a", "t");
    assert_eq!(fx.provider.id().as_str(), "builders");
    assert_eq!(fx.provider.flavour(), Flavour::Delegated);
    assert_eq!(fx.provider.classes(), vec![class()]);
    let foreign = ResourceClass::new(ClassId::new("other").unwrap(), Admission::BookAll, 0);
    let mut asks_foreign = request();
    asks_foreign.class = foreign.id().clone();
    let spec = js_provider::StartSpec::new(lease(1), asks_foreign, std::slice::from_ref(&foreign))
        .unwrap();
    assert_eq!(
        fx.provider.start(spec),
        StartAck::Refused(RefusalCode::UnsupportedClass)
    );
    let shared = ResourceClass::new(
        ClassId::new("shared").unwrap(),
        Admission::Book(std::collections::BTreeSet::from([js_types::Dim::CpuMillis])),
        0,
    );
    fx.provider = fx
        .provider
        .clone()
        .with_classes(vec![class(), shared.clone()]);
    let mut asks_shared = request();
    asks_shared.class = shared.id().clone();
    let spec = js_provider::StartSpec::new(lease(2), asks_shared, &fx.provider.classes()).unwrap();
    assert_eq!(
        fx.provider.start(spec),
        StartAck::Refused(RefusalCode::UnsupportedClass)
    );
    assert!(fx.provider.booked().is_empty());
}

/// Asserts that seating on a known free host books it, that seating a lease on another
/// host than it holds is refused, and that a seat of the same pair is a no-op.
#[test]
fn seat_on_a_known_host() {
    let fx = fixture(4);
    fx.join("a", "t");
    fx.join("b", "t");
    fx.provider.seat(&host("a"), lease(1), None).unwrap();
    fx.provider.seat(&host("a"), lease(1), None).unwrap();
    assert_eq!(
        fx.provider.seat(&host("b"), lease(1), None),
        Err(SeatError::Elsewhere {
            lease: lease(1),
            host: host("a")
        })
    );
    assert_eq!(fx.provider.booked().len(), 1);
}

/// Asserts that a start passes over a lost host for the next live one: host `a` is lost
/// and `b` live, and the lease lands on `b`. A defect that looked at the first host by
/// id only would answer no capacity while `b` sat free.
#[test]
fn start_skips_a_lost_host_for_a_live_one() {
    let mut fx = fixture(4);
    fx.join("a", "t");
    fx.pass_grace();
    let epoch = fx.join("b", "t");
    assert_eq!(epoch, 1);
    assert_eq!(started_on(&fx.provider.start(spec(1))), Some("b"));
}
