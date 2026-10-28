//! The wait list of whole-node requests and the `no-capable-host` refusal: which starts
//! are accepted and kept in order, which are refused at once, how a repeat of a request
//! id is a poll, and when a waiting request is dropped or refused. The registry is
//! driven directly, on a manual clock, with hand-built heartbeats.

mod support;

use std::time::Duration;

use js_agent_wire::Taint;
use js_provider::{Provider, RefusalCode, StartAck, StartSpec};
use js_registry::{DEFAULT_REGAIN_GRACE_MS, Exclusion, PoolConfig};
use js_types::{ProviderId, Request, RequestId, ResourceClass};
use support::*;

const GRACE: u64 = DEFAULT_REGAIN_GRACE_MS;

fn whole() -> ResourceClass {
    ResourceClass::whole_node()
}

fn whole_fixture() -> Fixture {
    let name = ProviderId::new("builders").unwrap();
    fixture_of(PoolConfig::new(name, whole(), pool_shape(), 4))
}

fn whole_request() -> Request {
    Request {
        class: whole().id().clone(),
        ..request()
    }
}

fn id(text: &str) -> RequestId {
    RequestId::new(text).unwrap()
}

/// A whole-node start of lease `sequence`, without a request id.
fn anonymous(sequence: u64) -> StartSpec {
    StartSpec::new(lease(sequence), whole_request(), &[whole()]).unwrap()
}

/// A whole-node start of lease `sequence` for request `name`.
fn named(sequence: u64, name: &str) -> StartSpec {
    anonymous(sequence).with_request_id(id(name))
}

fn refused(code: RefusalCode) -> StartAck {
    StartAck::Refused(code)
}

fn taint(fx: &Fixture, host: &str, epoch: u64) {
    let mut message = fx.heartbeat(host, epoch);
    message.taint = Some(Taint {
        reason: "check".into(),
        run_id: None,
        check: None,
    });
    fx.registry.heartbeat(&message, &cred("t")).unwrap();
}

fn advance(fx: &Fixture, ms: u64) {
    fx.clock.advance(Duration::from_millis(ms));
}

/// A pool with host `a` joined and lease 1 committed to it for request `holder`: the
/// host is busy.
fn busy() -> (Fixture, js_registry::RegisteredProvider, u64) {
    let fx = whole_fixture();
    let epoch = fx.join("a", "t");
    let mut provider = fx.registry.provider();
    assert_eq!(provider.start(named(1, "holder")), StartAck::Unknown);
    (fx, provider, epoch)
}

/// Asserts W8 with no host registered: a whole-node request is refused `no-capable-host`
/// with or without a request id and is not kept waiting, while a `BookAll` request on the
/// same empty pool keeps its `NoCapacity` (a host could still join, and the consumer
/// retries).
#[test]
fn no_registered_host_refuses_a_whole_request_and_not_a_bookall_one() {
    let fx = whole_fixture();
    let mut provider = fx.registry.provider();
    let none = refused(RefusalCode::NoCapableHost);
    assert_eq!(provider.start(anonymous(1)), none);
    assert_eq!(provider.start(named(2, "r1")), none);
    assert_eq!(fx.registry.queue_position(&id("r1")), None);
    let plain = fixture(4);
    let mut plain_provider = plain.registry.provider();
    let no_capacity = refused(RefusalCode::NoCapacity);
    assert_eq!(plain_provider.start(spec(1)), no_capacity);
    let with_id = spec(2).with_request_id(id("r1"));
    assert_eq!(plain_provider.start(with_id), no_capacity);
    assert_eq!(plain.registry.queue_position(&id("r1")), None);
}

/// Asserts that a request too large for the pool's shape is still refused `no-fit`, not
/// `no-capable-host`: the pool floor already says it can never fit.
#[test]
fn a_request_larger_than_the_pool_is_still_no_fit() {
    let fx = whole_fixture();
    fx.join("a", "t");
    let mut big = whole_request();
    big.resources = js_types::Resources::new(64_000, 1, 0, None).unwrap();
    let spec = StartSpec::new(lease(1), big, &[whole()]).unwrap();
    let mut provider = fx.registry.provider();
    assert_eq!(provider.start(spec), refused(RefusalCode::NoFit));
}

/// Asserts that a pool whose only host is tainted refuses a whole request
/// `no-capable-host` at once, with a request id too, and keeps no entry.
#[test]
fn a_tainted_only_host_refuses_a_whole_request() {
    let fx = whole_fixture();
    let epoch = fx.join("a", "t");
    taint(&fx, "a", epoch);
    let mut provider = fx.registry.provider();
    let none = refused(RefusalCode::NoCapableHost);
    assert_eq!(provider.start(anonymous(1)), none);
    assert_eq!(provider.start(named(2, "r1")), none);
    assert_eq!(fx.registry.queue_position(&id("r1")), None);
}

/// Asserts that a busy host is capable: with a request id the start is accepted
/// (`Unknown`, the API's pending) and the request waits at the front; without one it
/// answers `NoCapacity` as before and waits nowhere.
#[test]
fn a_busy_host_queues_a_request_with_an_id_and_refuses_one_without() {
    let (fx, mut provider, _) = busy();
    assert_eq!(provider.start(named(2, "r1")), StartAck::Unknown);
    assert_eq!(fx.registry.queue_position(&id("r1")), Some(0));
    assert_eq!(
        provider.start(anonymous(3)),
        refused(RefusalCode::NoCapacity)
    );
}

/// Asserts that a lost host inside the retention window is capable: the request is
/// queued, not refused.
#[test]
fn a_lost_host_inside_retention_still_queues() {
    let fx = whole_fixture();
    fx.join("a", "t");
    fx.pass_grace();
    assert_eq!(fx.registry.hosts()[0].state, js_registry::HostState::Lost);
    let mut provider = fx.registry.provider();
    // The lost host is not bookable, so the request cannot be placed, but it is capable.
    assert_eq!(provider.start(named(1, "r1")), StartAck::Unknown);
    assert_eq!(fx.registry.queue_position(&id("r1")), Some(0));
}

/// Asserts the queue order: requests are placed in the order they were first seen across
/// interleaved ids, a repeat of an id keeps its place and adds no second entry (cancelling
/// one id leaves exactly the others, in order).
#[test]
fn the_queue_is_first_seen_order_and_a_repeat_is_a_poll() {
    let (fx, mut provider, _) = busy();
    for (sequence, name) in [(2, "a"), (3, "b"), (4, "a"), (5, "b"), (6, "c"), (7, "a")] {
        assert_eq!(provider.start(named(sequence, name)), StartAck::Unknown);
    }
    let at = |name: &str| fx.registry.queue_position(&id(name));
    assert_eq!((at("a"), at("b"), at("c")), (Some(0), Some(1), Some(2)));
    assert!(fx.registry.cancel_request(&id("a")));
    assert_eq!((at("a"), at("b"), at("c")), (None, Some(0), Some(1)));
}

/// Asserts that a cancel removes the request at once and says whether it was waiting.
#[test]
fn cancel_removes_a_waiting_request_at_once() {
    let (fx, mut provider, _) = busy();
    assert_eq!(provider.start(named(2, "r1")), StartAck::Unknown);
    assert!(fx.registry.cancel_request(&id("r1")));
    assert_eq!(fx.registry.queue_position(&id("r1")), None);
    assert!(!fx.registry.cancel_request(&id("r1")));
    assert!(!fx.registry.cancel_request(&id("never")));
}

/// Asserts the client-death rule: an entry not presented again for `regain_grace` is
/// dropped exactly at that instant (kept one millisecond before), and a presentation
/// before then renews the whole grace.
#[test]
fn an_entry_not_presented_for_regain_grace_is_dropped_at_the_boundary() {
    let (fx, mut provider, _) = busy();
    assert_eq!(provider.start(named(2, "r1")), StartAck::Unknown);
    assert_eq!(provider.start(named(3, "r2")), StartAck::Unknown);
    advance(&fx, GRACE - 1);
    assert_eq!(fx.registry.queue_position(&id("r1")), Some(0));
    assert_eq!(provider.start(named(4, "r2")), StartAck::Unknown);
    advance(&fx, 1);
    assert_eq!(fx.registry.queue_position(&id("r1")), None);
    assert_eq!(fx.registry.queue_position(&id("r2")), Some(0));
    advance(&fx, GRACE - 2);
    assert_eq!(fx.registry.queue_position(&id("r2")), Some(0));
    advance(&fx, 1);
    assert_eq!(fx.registry.queue_position(&id("r2")), None);
}

/// Asserts that a dropped request that is presented again waits again, at the back.
#[test]
fn a_dropped_request_presented_again_waits_at_the_back() {
    let (fx, mut provider, _) = busy();
    assert_eq!(provider.start(named(2, "r1")), StartAck::Unknown);
    advance(&fx, GRACE - 1);
    assert_eq!(provider.start(named(3, "r2")), StartAck::Unknown);
    advance(&fx, 1);
    assert_eq!(provider.start(named(4, "r1")), StartAck::Unknown);
    assert_eq!(fx.registry.queue_position(&id("r2")), Some(0));
    assert_eq!(fx.registry.queue_position(&id("r1")), Some(1));
}

/// Asserts that the last capable host leaving keeps a waiting request waiting for
/// `regain_grace` and then refuses it `no-capable-host`: still waiting one millisecond
/// before, refused exactly at the boundary, and no longer on the list.
#[test]
fn the_last_capable_host_leaving_refuses_after_regain_grace() {
    let (fx, mut provider, epoch) = busy();
    assert_eq!(provider.start(named(2, "r1")), StartAck::Unknown);
    taint(&fx, "a", epoch);
    assert_eq!(provider.start(named(3, "r1")), StartAck::Unknown);
    advance(&fx, GRACE - 1);
    assert_eq!(provider.start(named(4, "r1")), StartAck::Unknown);
    assert_eq!(fx.registry.queue_position(&id("r1")), Some(0));
    advance(&fx, 1);
    // The poll one millisecond ago renewed the entry; the grace for the host is counted
    // from the first poll that saw none.
    assert_eq!(
        provider.start(named(5, "r1")),
        refused(RefusalCode::NoCapableHost)
    );
    assert_eq!(fx.registry.queue_position(&id("r1")), None);
}

/// Asserts that the grace for a missing host starts again once a host is capable: a host
/// joins before the grace ends, the wait is cleared, and a second loss waits the whole
/// grace again.
#[test]
fn a_capable_host_clears_the_wait_for_one() {
    let (fx, mut provider, epoch) = busy();
    assert_eq!(provider.start(named(2, "r1")), StartAck::Unknown);
    taint(&fx, "a", epoch);
    assert_eq!(provider.start(named(3, "r1")), StartAck::Unknown);
    advance(&fx, GRACE / 3);
    // The new host is draining, so it takes nothing yet, but it could be given whole.
    let mut draining = hello("b");
    draining.phase = js_agent_wire::Phase::Draining;
    let b = fx.hello(&draining, &cred("t")).unwrap().epoch;
    assert_eq!(provider.start(named(4, "r1")), StartAck::Unknown);
    taint(&fx, "b", b);
    assert_eq!(provider.start(named(5, "r1")), StartAck::Unknown);
    advance(&fx, GRACE - 1);
    assert_eq!(provider.start(named(6, "r1")), StartAck::Unknown);
    advance(&fx, 1);
    assert_eq!(
        provider.start(named(7, "r1")),
        refused(RefusalCode::NoCapableHost)
    );
}

/// Asserts that a request placed on a host leaves the wait list in the same call.
#[test]
fn a_placed_request_leaves_the_wait_list() {
    let (fx, mut provider, epoch) = busy();
    assert_eq!(provider.start(named(2, "r1")), StartAck::Unknown);
    assert_eq!(fx.registry.queue_position(&id("r1")), Some(0));
    provider.stop(&lease(1));
    for _ in 0..2 {
        fx.beat("a", epoch);
    }
    assert_eq!(provider.start(named(3, "r1")), StartAck::Unknown);
    assert_eq!(provider.booked().get(&host("a")), Some(&lease(3)));
    assert_eq!(fx.registry.queue_position(&id("r1")), None);
}

/// Asserts that the record of failed prepares is kept under the request id: two requests
/// with equal content and different ids have separate records and exclusions; the same
/// id with different content shares one; and a request with an id has no record under
/// its content.
#[test]
fn the_request_record_is_keyed_by_the_request_id() {
    let fx = whole_fixture();
    let epoch = fx.join("a", "t");
    let mut provider = fx.registry.provider();
    assert_eq!(provider.start(named(1, "r1")), StartAck::Unknown);
    let mut message = fx.heartbeat("a", epoch);
    message.exclusive_refused = Some(js_agent_wire::ExclusiveRefusal {
        lease_id: "1-1".into(),
        code: js_agent_wire::RefusalReason::Unsupported,
        detail: "x".into(),
    });
    fx.registry.heartbeat(&message, &cred("t")).unwrap();
    for _ in 0..2 {
        fx.beat("a", epoch);
    }
    let kept = fx.registry.request_record(&id("r1")).unwrap();
    assert_eq!(kept.prepare_failures, 1);
    assert_eq!(kept.exclusions.get(&host("a")), Some(&Exclusion::ForLife));
    assert_eq!(fx.registry.request_record(&id("r2")), None);
    assert_eq!(fx.registry.request_record(&whole_request()), None);
    // The same id with other content meets the same exclusion; an equal request under
    // another id does not.
    let mut other = whole_request();
    other.max_wall = Duration::from_secs(1);
    let same_id = StartSpec::new(lease(2), other, &[whole()])
        .unwrap()
        .with_request_id(id("r1"));
    assert_eq!(provider.start(same_id), refused(RefusalCode::NoCapableHost));
    assert_eq!(provider.start(named(3, "r2")), StartAck::Unknown);
    assert_eq!(provider.booked().get(&host("a")), Some(&lease(3)));
}

/// Asserts that a start without a request id keeps the content key: equal requests share
/// one record, as before.
#[test]
fn a_start_without_an_id_is_keyed_by_its_content() {
    let fx = whole_fixture();
    let epoch = fx.join("a", "t");
    let mut provider = fx.registry.provider();
    assert_eq!(provider.start(anonymous(1)), StartAck::Unknown);
    let mut message = fx.heartbeat("a", epoch);
    message.exclusive_refused = Some(js_agent_wire::ExclusiveRefusal {
        lease_id: "1-1".into(),
        code: js_agent_wire::RefusalReason::SetupFailed,
        detail: "x".into(),
    });
    fx.registry.heartbeat(&message, &cred("t")).unwrap();
    assert_eq!(
        fx.registry
            .request_record(&whole_request())
            .map(|record| record.prepare_failures),
        Some(1)
    );
}
