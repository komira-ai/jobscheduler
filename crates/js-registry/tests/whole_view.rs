//! What an operator and a lease holder can read of the marks: which hosts are set aside
//! or held, for which request and until when, and the counts of both and of the wait
//! list. The registry is driven directly on a manual clock.

mod support;

use std::time::Duration;

use js_lease::Clock;
use js_provider::{Provider, StartAck, StartSpec};
use js_registry::{
    HostMark, HostView, PoolConfig, RegisteredProvider, Registry, WholeCounts, WholeNodeConfig,
};
use js_types::{Admission, ClassId, ProviderId, Request, RequestId, ResourceClass};
use support::*;

fn whole() -> ResourceClass {
    ResourceClass::whole_node()
}

fn share_class() -> ResourceClass {
    ResourceClass::new(ClassId::new("share").unwrap(), Admission::BookAll, 0)
}

fn id(text: &str) -> RequestId {
    RequestId::new(text).unwrap()
}

/// Two hosts, `a` and `b`, joined to a dedicated pool that reserves at once.
fn world() -> (Fixture, RegisteredProvider) {
    let name = ProviderId::new("builders").unwrap();
    let config = PoolConfig::new(name, whole(), pool_shape(), 2);
    let config = config
        .with_whole_node(WholeNodeConfig::dedicated(2))
        .unwrap();
    let fx = fixture_of(config);
    let provider = fx
        .registry
        .provider()
        .with_classes(vec![share_class(), whole()]);
    fx.join("a", "t");
    fx.join("b", "t");
    (fx, provider)
}

fn share(sequence: u64) -> StartSpec {
    let request = Request {
        class: ClassId::new("share").unwrap(),
        ..request()
    };
    StartSpec::new(lease(sequence), request, &[share_class()]).unwrap()
}

fn ask(sequence: u64, name: &str) -> StartSpec {
    let request = Request {
        class: whole().id().clone(),
        max_wall: Duration::from_secs(600),
        ..request()
    };
    let spec = StartSpec::new(lease(sequence), request, &[whole()]).unwrap();
    spec.with_request_id(id(name))
}

fn view(registry: &Registry, name: &str) -> HostView {
    let hosts = registry.hosts();
    hosts.into_iter().find(|h| h.id == host(name)).unwrap()
}

fn marks(registry: &Registry) -> Vec<Option<HostMark>> {
    registry.hosts().iter().map(|h| h.mark).collect()
}

/// Asserts the view of a host that is draining for a waiting request: the mark reads
/// `reserved`, names the request and carries the deadline of the reservation, while the
/// other host reads none of the three; `is_reserved` and the counts agree. Before the
/// request every host reads none. It catches a view that shows no mark, a deadline that
/// is not the reservation's, and a request named on a host that is not its.
#[test]
fn a_draining_host_shows_its_request_and_deadline() {
    let (fx, mut provider) = world();
    assert_eq!(marks(&fx.registry), vec![None, None]);
    assert_eq!(fx.registry.whole_counts(), WholeCounts::default());

    assert!(matches!(provider.start(share(1)), StartAck::Started { .. }));
    assert!(matches!(provider.start(share(2)), StartAck::Started { .. }));
    assert_eq!(provider.start(ask(3, "r1")), StartAck::Unknown);

    let reserved = fx.registry.reservations();
    assert_eq!(reserved.len(), 1);
    let on = view(&fx.registry, reserved[0].host.as_str());
    assert_eq!(on.mark, Some(HostMark::Reserved));
    assert_eq!(on.reserved_for, Some(id("r1")));
    assert_eq!(on.drain_deadline, Some(reserved[0].deadline));
    assert!(reserved[0].deadline > fx.clock.now());
    let other = if reserved[0].host == host("a") {
        "b"
    } else {
        "a"
    };
    let off = view(&fx.registry, other);
    assert_eq!(
        (off.mark, off.reserved_for, off.drain_deadline),
        (None, None, None)
    );

    assert!(fx.registry.is_reserved(&reserved[0].host));
    assert!(!fx.registry.is_reserved(&host(other)));
    let counts = fx.registry.whole_counts();
    assert_eq!(
        counts,
        WholeCounts {
            reserved: 1,
            held: 0,
            queued: 1
        }
    );
}

/// Asserts that a quiet host assigned to a request reads `reserved` with its request and
/// no deadline (it drains nothing), and that once the request commits the host reads
/// `held` with neither request nor deadline, and the counts move from reserved to held
/// and from queued to none. It catches an assigned mark that shows a meaningless
/// deadline, a held host that still names the request, and a held host with no mark.
#[test]
fn an_assigned_host_has_no_deadline_and_a_committed_one_reads_held() {
    let (fx, mut provider) = world();
    provider.start(share(1));
    provider.start(share(2));
    assert_eq!(provider.start(ask(3, "r1")), StartAck::Unknown);
    let draining = fx.registry.reservations()[0].host.clone();
    let booked = provider.booked();
    let (quiet, leased) = booked.iter().find(|(h, _)| **h != draining).unwrap();
    provider.stop(leased);
    provider.observe(leased);

    let mark = fx.registry.reservations().remove(0);
    assert_eq!((mark.host.clone(), mark.assigned), (quiet.clone(), true));
    let on = view(&fx.registry, quiet.as_str());
    assert_eq!(
        (on.mark, on.reserved_for.clone(), on.drain_deadline),
        (Some(HostMark::Reserved), Some(id("r1")), None)
    );
    assert_eq!(view(&fx.registry, draining.as_str()).mark, None);

    assert_eq!(provider.start(ask(3, "r1")), StartAck::Unknown);
    let held = view(&fx.registry, quiet.as_str());
    assert_eq!(
        (held.mark, held.reserved_for, held.drain_deadline),
        (Some(HostMark::Held), None, None)
    );
    assert_eq!(
        fx.registry.whole_counts(),
        WholeCounts {
            reserved: 0,
            held: 1,
            queued: 0
        }
    );
}

/// Asserts that a lease reads the host it is booked on, and a lease that is booked
/// nowhere reads none. It catches `host_of` answering a host for a lease the registry
/// does not hold.
#[test]
fn a_lease_reads_its_host() {
    let (fx, mut provider) = world();
    provider.start(share(1));
    let booked = provider.booked();
    let (on, leased) = booked.iter().next().unwrap();
    assert_eq!(fx.registry.host_of(leased), Some(on.clone()));
    assert_eq!(fx.registry.host_of(&lease(9)), None);
}
