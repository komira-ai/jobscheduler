//! The limits of the whole-node wait list and of a request id: how many requests wait,
//! that one request id holds at most one host, and that the gpu-host class is untouched
//! by the whole-node records. The registry is driven directly, on a manual clock.
//!
//! The wait list is in first-seen order but placement does not follow it yet, so nothing
//! here asserts that the front request gets a freed host.

mod support;

use js_agent_wire::{ExclusiveRefusal, RefusalReason};
use js_provider::{Provider, RefusalCode, StartAck, StartSpec};
use js_registry::{DEFAULT_RESERVE_BACKOFF_MS, MAX_WAITING_REQUESTS, PoolConfig, WholeNodeConfig};
use js_types::{ProviderId, Request, RequestId, ResourceClass, Resources, Shape};
use support::*;

fn whole() -> ResourceClass {
    ResourceClass::whole_node()
}

fn config() -> PoolConfig {
    let name = ProviderId::new("builders").unwrap();
    PoolConfig::new(name, whole(), pool_shape(), 4)
}

fn id(text: &str) -> RequestId {
    RequestId::new(text).unwrap()
}

fn whole_request() -> Request {
    Request {
        class: whole().id().clone(),
        ..request()
    }
}

/// A whole-node start of lease `sequence` for request `name`.
fn named(sequence: u64, name: &str) -> StartSpec {
    StartSpec::new(lease(sequence), whole_request(), &[whole()])
        .unwrap()
        .with_request_id(id(name))
}

fn no_capacity() -> StartAck {
    StartAck::Refused(RefusalCode::NoCapacity)
}

/// A pool with host `a` joined and lease 1 committed to it for request `holder`.
fn busy() -> (Fixture, js_registry::RegisteredProvider, u64) {
    let fx = fixture_of(config());
    let epoch = fx.join("a", "t");
    let mut provider = fx.registry.provider();
    assert_eq!(provider.start(named(1, "holder")), StartAck::Unknown);
    (fx, provider, epoch)
}

fn waiter(n: usize) -> String {
    format!("w{n}")
}

/// Asserts the cap on the wait list, below and at it: with one fewer than the cap
/// waiting, a new id still enters and waits at the back; with the cap waiting, a new id
/// is answered `NoCapacity` and is not entered; a request already waiting keeps its place
/// and still polls `Unknown`; and a place freed by a cancel is taken by the next new id.
#[test]
fn a_new_id_past_the_cap_is_not_entered() {
    let (fx, mut provider, _) = busy();
    let below = MAX_WAITING_REQUESTS - 1;
    for n in 0..below {
        let sequence = 10 + n as u64;
        let ack = provider.start(named(sequence, &waiter(n)));
        assert_eq!(ack, StartAck::Unknown);
    }
    assert_eq!(
        fx.registry.queue_position(&id(&waiter(below - 1))),
        Some(below - 1)
    );
    // The last place under the cap is taken.
    let last = 10 + below as u64;
    assert_eq!(provider.start(named(last, "last")), StartAck::Unknown);
    assert_eq!(fx.registry.queue_position(&id("last")), Some(below));
    // The list is full: a new id is turned away and not kept; a waiting one polls on.
    let over = last + 1;
    assert_eq!(provider.start(named(over, "over")), no_capacity());
    assert_eq!(fx.registry.queue_position(&id("over")), None);
    assert_eq!(provider.start(named(10, &waiter(0))), StartAck::Unknown);
    assert_eq!(fx.registry.queue_position(&id(&waiter(0))), Some(0));
    // A cancel frees one place.
    assert!(fx.registry.cancel_request(&id(&waiter(5))));
    assert_eq!(provider.start(named(over + 1, "over")), StartAck::Unknown);
    assert_eq!(
        fx.registry.queue_position(&id("over")),
        Some(MAX_WAITING_REQUESTS - 1)
    );
}

/// Asserts that a whole-node request with no capable host is refused `no-capable-host`
/// rather than `NoCapacity` also when the list is full: the cap only turns away requests
/// that would have waited.
#[test]
fn a_full_list_still_refuses_what_no_host_could_serve() {
    let (fx, mut provider, epoch) = busy();
    for n in 0..MAX_WAITING_REQUESTS {
        let sequence = 10 + n as u64;
        assert_eq!(
            provider.start(named(sequence, &waiter(n))),
            StartAck::Unknown
        );
    }
    let mut message = fx.heartbeat("a", epoch);
    message.taint = Some(js_agent_wire::Taint {
        reason: "check".into(),
        run_id: None,
        check: None,
    });
    fx.registry.heartbeat(&message, &cred("t")).unwrap();
    let none = StartAck::Refused(RefusalCode::NoCapableHost);
    assert_eq!(provider.start(named(5_000, "late")), none);
}

/// Asserts the one-host rule for a request id while the first lease is committed and not
/// yet ready: a start of the same id under a new lease books nothing, waits (`NoCapacity`)
/// and is not entered on the list, though another host is free; it books once the first
/// lease has ended.
#[test]
fn a_request_id_holds_one_host_while_committed() {
    let (fx, mut provider, epoch) = busy();
    fx.join("b", "t");
    assert_eq!(provider.start(named(2, "holder")), no_capacity());
    assert_eq!(provider.booked().len(), 1);
    assert_eq!(provider.booked().get(&host("a")), Some(&lease(1)));
    assert_eq!(fx.registry.queue_position(&id("holder")), None);
    // The first lease keeps answering as before.
    assert_eq!(provider.start(named(1, "holder")), StartAck::Unknown);
    provider.stop(&lease(1));
    for _ in 0..2 {
        fx.beat("a", epoch);
    }
    assert_eq!(provider.start(named(2, "holder")), StartAck::Unknown);
    assert_eq!(provider.booked().len(), 1);
}

/// Asserts the same for a lease that is ready: the id still holds its host, and another
/// id is placed on the free one.
#[test]
fn a_request_id_holds_one_host_while_ready() {
    let (fx, mut provider, epoch) = busy();
    fx.join("b", "t");
    let mut message = fx.heartbeat("a", epoch);
    message.exclusive_ready = Some(lease(1).to_string());
    fx.registry.heartbeat(&message, &cred("t")).unwrap();
    assert_eq!(provider.start(named(2, "holder")), no_capacity());
    assert_eq!(provider.booked().len(), 1);
    assert_eq!(provider.start(named(3, "other")), StartAck::Unknown);
    assert_eq!(provider.booked().get(&host("b")), Some(&lease(3)));
}

/// Asserts that a start without an id is not held to one host: two equal anonymous
/// requests are two consumers and both book.
#[test]
fn anonymous_equal_requests_each_book_a_host() {
    let fx = fixture_of(config());
    fx.join("a", "t");
    fx.join("b", "t");
    let mut provider = fx.registry.provider();
    let anonymous =
        |sequence| StartSpec::new(lease(sequence), whole_request(), &[whole()]).unwrap();
    assert_eq!(provider.start(anonymous(1)), StartAck::Unknown);
    assert_eq!(provider.start(anonymous(2)), StartAck::Unknown);
    assert_eq!(provider.booked().len(), 2);
}

/// A gpu pool where request `r1` used up its one try as a whole-node request on host
/// `a`, which restored; the provider serves both classes.
fn spent() -> (Fixture, js_registry::RegisteredProvider, u64) {
    let settings = WholeNodeConfig {
        max_replace: 1,
        ..WholeNodeConfig::dedicated(4)
    };
    let a100 = Some("a100".to_string());
    let shape = Shape::new(
        Resources::new(4000, 8 << 30, 1, a100.clone()).unwrap(),
        map(&[("os", "macos")]),
    );
    let name = ProviderId::new("builders").unwrap();
    let config = PoolConfig::new(name, whole(), shape, 4)
        .with_whole_node(settings)
        .unwrap();
    let fx = fixture_of(config);
    let mut joining = hello("a");
    joining.shape.gpus = 1;
    joining.shape.gpu_class = a100;
    let epoch = fx.hello(&joining, &cred("t")).unwrap().epoch;
    let mut provider = fx
        .registry
        .provider()
        .with_classes(vec![whole(), ResourceClass::gpu_host()]);
    assert_eq!(provider.start(named(1, "r1")), StartAck::Unknown);
    let mut message = fx.heartbeat("a", epoch);
    message.exclusive_refused = Some(ExclusiveRefusal {
        lease_id: lease(1).to_string(),
        code: RefusalReason::SetupFailed,
        detail: String::new(),
    });
    fx.registry.heartbeat(&message, &cred("t")).unwrap();
    assert!(fx.registry.not_ready(&id("r1")).is_some());
    for _ in 0..2 {
        fx.beat("a", epoch);
    }
    (fx, provider, epoch)
}

/// A gpu-host start under request id `r1`.
fn gpu_start(sequence: u64) -> StartSpec {
    let gpu = Request {
        class: ResourceClass::gpu_host().id().clone(),
        resources: Resources::new(1000, 1, 1, Some("a100".into())).unwrap(),
        ..whole_request()
    };
    StartSpec::new(lease(sequence), gpu, &[ResourceClass::gpu_host()])
        .unwrap()
        .with_request_id(id("r1"))
}

/// Asserts that the spent record of a whole-node request does not reach the gpu-host
/// class: after request id `r1` used up its tries, with the back-off on the host over, a
/// gpu-host start under the same id is placed (`Started`, no `exclusive`) and is not
/// refused; and it does not consume the record, so a whole-node start under the id is
/// still refused `NoCapacity` once.
#[test]
fn a_spent_whole_node_record_does_not_refuse_a_gpu_host_start() {
    let (fx, mut provider, epoch) = spent();
    fx.clock
        .advance(std::time::Duration::from_millis(DEFAULT_RESERVE_BACKOFF_MS));
    fx.beat("a", epoch);
    let ack = provider.start(gpu_start(2));
    assert_eq!(started_on(&ack), Some("a"), "{ack:?}");
    assert!(fx.registry.not_ready(&id("r1")).is_some());
    provider.stop(&lease(2));
    assert_eq!(provider.start(named(3, "r1")), no_capacity());
    assert_eq!(fx.registry.not_ready(&id("r1")), None);
}

/// Asserts that the host a whole-node request is kept off during the back-off is open to
/// a gpu-host start under the same request id.
#[test]
fn a_whole_node_back_off_does_not_close_a_host_to_a_gpu_host_start() {
    let (_fx, mut provider, _) = spent();
    let ack = provider.start(gpu_start(2));
    assert_eq!(started_on(&ack), Some("a"), "{ack:?}");
}
