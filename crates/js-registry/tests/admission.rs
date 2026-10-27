//! What the registry admits or refuses at hello, and what a heartbeat does to a session.

mod support;

use js_agent_wire::{Phase, refusal};
use js_pool_static::SeatError;
use js_registry::{HostState, Refusal};
use support::*;

/// Asserts that a host claiming less than the pool promises is refused with its own
/// code and that nothing is created: a pool that admitted a smaller machine would book
/// it as the shape it does not offer. A host missing a capability the pool promises, or
/// holding it with another value, is refused the same way.
#[test]
fn wrong_shape_refused() {
    let fx = fixture(4);
    let mut small = hello("small");
    small.shape.cpu_millis = 3999;
    let mut no_os = hello("no-os");
    no_os.capabilities.clear();
    let mut linux = hello("linux");
    linux.capabilities = map(&[("os", "linux")]);
    for bad in [small, no_os, linux] {
        let refused = fx.registry.hello(&bad, &cred("t")).unwrap_err();
        assert_eq!(refused, Refusal::ShapeNotAccepted);
        assert_eq!(refused.code(), refusal::SHAPE_NOT_ACCEPTED);
    }
    assert!(fx.registry.hosts().is_empty());
}

/// Asserts that a host claiming more than the pool promises is booked as the pool's
/// shape: a request above the pool's shape but within the claim finds no fit. A defect
/// that booked the claim would let an agent inflate what a lease holds.
#[test]
fn booked_shape_is_the_pool_shape_not_the_claim() {
    let mut fx = fixture(4);
    fx.join("big", "t");
    let mut wanted = request();
    wanted.resources = js_types::Resources::new(6000, 1, 0, None).unwrap();
    let spec = js_provider::StartSpec::new(lease(1), wanted, &[class()]).unwrap();
    let ack = js_provider::Provider::start(&mut fx.provider, spec);
    assert_eq!(
        ack,
        js_provider::StartAck::Refused(js_provider::RefusalCode::NoFit)
    );
    let listed = fx.registry.hosts();
    assert_eq!(listed[0].shape, pool_shape());
}

/// Asserts that a full pool refuses a host it does not know, and still takes the hello
/// of a host it already holds: a restarted agent must get back in when the pool is at
/// its limit. The count includes a host that is only awaiting after a restart.
#[test]
fn pool_full_refused_but_rehello_of_known_host_ok() {
    let fx = fixture(2);
    fx.join("a", "t");
    fx.join("b", "t");
    let full = fx.registry.hello(&hello("c"), &cred("t")).unwrap_err();
    assert_eq!(full, Refusal::PoolFull);
    assert_eq!(full.code(), refusal::POOL_FULL);
    assert_eq!(fx.registry.hosts().len(), 2);
    fx.pass_grace();
    assert_eq!(fx.join("a", "t"), 2);
}

/// Asserts that a seated host counts toward the limit although the seat itself is never
/// refused for room, so a restart cannot drop a lease because the pool is full.
#[test]
fn seat_ignores_the_limit_but_counts_toward_it() {
    let fx = fixture(1);
    fx.provider.seat(&host("a"), lease(1), None).unwrap();
    fx.provider.seat(&host("b"), lease(2), None).unwrap();
    let full = fx.registry.hello(&hello("c"), &cred("t")).unwrap_err();
    assert_eq!(full, Refusal::PoolFull);
}

/// Asserts that a second hello for a live host id is refused and changes nothing: the
/// first session keeps its epoch and keeps heartbeating. Without the rule an id could
/// be taken over by anyone holding the pool's token.
#[test]
fn second_hello_while_live_refused() {
    let fx = fixture(4);
    let epoch = fx.join("a", "t");
    let refused = fx.registry.hello(&hello("a"), &cred("t")).unwrap_err();
    assert_eq!(refused, Refusal::HostLive);
    assert_eq!(refused.code(), refusal::HOST_LIVE);
    fx.beat("a", epoch);
    assert_eq!(fx.registry.hosts()[0].epoch, epoch);
}

/// Asserts that silence for exactly the grace period is still live and one millisecond
/// more is not, and that a hello after it replaces the session with the next epoch, so
/// the old session's heartbeat is stale. A defect that kept the epoch would let a
/// zombie agent and its replacement both look current.
#[test]
fn hello_after_grace_replaces_and_bumps_epoch() {
    let fx = fixture(4);
    let first = fx.join("a", "t");
    fx.clock.advance(std::time::Duration::from_millis(
        js_agent_wire::DEFAULT_GRACE_MS,
    ));
    assert_eq!(fx.registry.hosts()[0].state, HostState::Live);
    assert_eq!(
        fx.registry.hello(&hello("a"), &cred("t")).unwrap_err(),
        Refusal::HostLive
    );
    fx.clock.advance(std::time::Duration::from_millis(1));
    assert_eq!(fx.registry.hosts()[0].state, HostState::Lost);
    let second = fx.join("a", "t");
    assert_eq!(second, first + 1);
    assert_eq!(fx.registry.hosts()[0].state, HostState::Live);
    let stale = fx
        .registry
        .heartbeat(&heartbeat("a", first), &cred("t"))
        .unwrap_err();
    assert_eq!(stale, Refusal::StaleEpoch);
    assert_eq!(stale.code(), refusal::STALE_EPOCH);
    fx.beat("a", second);
}

/// Asserts that a host id is bound to the first credential it presented: another token
/// is refused, whether the host is live or lost, and before the live check so a holder of
/// a different token learns nothing about liveness. The first credential still works.
#[test]
fn host_bound_to_first_credential() {
    let fx = fixture(4);
    fx.join("a", "token-one");
    let live = fx
        .registry
        .hello(&hello("a"), &cred("token-two"))
        .unwrap_err();
    assert_eq!(live, Refusal::HostBound);
    assert_eq!(live.code(), refusal::HOST_BOUND);
    fx.pass_grace();
    let lost = fx
        .registry
        .hello(&hello("a"), &cred("token-two"))
        .unwrap_err();
    assert_eq!(lost, Refusal::HostBound);
    assert_eq!(fx.join("a", "token-one"), 2);
}

/// Asserts that a hello with a label under the reserved prefix is refused with the
/// reserved-label code and creates no host: the prefix is the scheduler's alone.
#[test]
fn reserved_label_refused() {
    let fx = fixture(4);
    let mut claimed = hello("a");
    claimed
        .labels
        .insert("jobscheduler.dev/taint".into(), "none".into());
    let refused = fx.registry.hello(&claimed, &cred("t")).unwrap_err();
    assert_eq!(refused.code(), refusal::RESERVED_LABEL);
    assert!(fx.registry.hosts().is_empty());
}

/// Asserts the remaining refusals of a hello, each with its code and no host created: a
/// pool this registry does not serve, a host id the scheduler cannot hold, a protocol
/// no major of which is shared, and a hello that breaks a wire bound.
#[test]
fn other_hellos_refused_with_their_codes() {
    let fx = fixture(4);
    let mut elsewhere = hello("a");
    elsewhere.pool = "other".into();
    let mut upper = hello("a");
    upper.host_id = "Mac-01".into();
    let mut future = hello("a");
    future.protocol = js_agent_wire::ProtocolRange { min: 9, max: 9 };
    let mut empty = hello("a");
    empty.agent_version = String::new();
    let cases = [
        (elsewhere, refusal::UNKNOWN_POOL),
        (upper, refusal::INVALID_HELLO),
        (future, refusal::UNSUPPORTED_PROTOCOL),
        (empty, refusal::INVALID_HELLO),
    ];
    for (bad, code) in cases {
        let refused = fx.registry.hello(&bad, &cred("t")).unwrap_err();
        assert_eq!(refused.code(), code);
        assert!(!refused.to_string().contains("Mac-01"));
    }
    assert!(fx.registry.hosts().is_empty());
}

/// Asserts that a heartbeat is refused for a host with no session, for a malformed body
/// and for a host id that is not valid, each with its code, and that a heartbeat from a
/// lost host with the current epoch revives it.
#[test]
fn heartbeat_refusals_and_revival() {
    let fx = fixture(4);
    let unknown = fx
        .registry
        .heartbeat(&heartbeat("a", 1), &cred("t"))
        .unwrap_err();
    assert_eq!(unknown, Refusal::UnknownHost);
    assert_eq!(unknown.code(), refusal::UNKNOWN_HOST);
    let upper = fx
        .registry
        .heartbeat(&heartbeat("A", 1), &cred("t"))
        .unwrap_err();
    assert_eq!(upper.code(), refusal::INVALID_HEARTBEAT);
    assert_eq!(upper.to_string(), "the host id is not a valid host id");
    let epoch = fx.join("a", "t");
    let mut broken = heartbeat("a", epoch);
    broken.held = vec![js_agent_wire::HeldRun {
        run_id: String::new(),
        state: js_agent_wire::HeldState::Running,
    }];
    let invalid = fx.registry.heartbeat(&broken, &cred("t")).unwrap_err();
    assert_eq!(invalid.code(), refusal::INVALID_HEARTBEAT);
    fx.pass_grace();
    assert_eq!(fx.registry.hosts()[0].state, HostState::Lost);
    fx.beat("a", epoch);
    assert_eq!(fx.registry.hosts()[0].state, HostState::Live);
}

/// Asserts that a heartbeat from a credential other than the one the host id is bound to
/// is refused as host-bound, whatever epoch it names, and changes nothing: the epoch is a
/// small counter anyone can guess, so it cannot be the only proof of a session. The
/// refused beat must not refresh the last-heard time (the host still goes lost on its own
/// silence) nor change the announced phase; the bound credential still works.
#[test]
fn heartbeat_from_another_credential_refused_and_changes_nothing() {
    let fx = fixture(4);
    let epoch = fx.join("a", "t");
    let three_quarters = std::time::Duration::from_millis(js_agent_wire::DEFAULT_GRACE_MS / 4 * 3);
    fx.clock.advance(three_quarters);
    let mut intruder = heartbeat("a", epoch);
    intruder.phase = Phase::Draining;
    let other = cred("token-two");
    let refused = fx.registry.heartbeat(&intruder, &other).unwrap_err();
    assert_eq!(refused, Refusal::HostBound);
    assert_eq!(refused.code(), refusal::HOST_BOUND);
    let guessed = heartbeat("a", epoch + 7);
    let wrong_epoch = fx.registry.heartbeat(&guessed, &other).unwrap_err();
    assert_eq!(wrong_epoch, Refusal::HostBound);
    fx.clock.advance(three_quarters);
    let view = &fx.registry.hosts()[0];
    assert_eq!(view.state, HostState::Lost);
    assert_eq!(view.phase, Phase::Serving);
    assert_eq!(view.epoch, epoch);
    fx.beat("a", epoch);
    assert_eq!(fx.registry.hosts()[0].state, HostState::Live);
}

/// Asserts that the listing says what an operator needs: the id, state, phase, the lease
/// that holds the host, the labels and agent version the host announced, and the epoch.
#[test]
fn hosts_lists_what_the_operator_needs() {
    let mut fx = fixture(4);
    let epoch = fx.join("a", "t");
    fx.join("b", "t");
    let ack = js_provider::Provider::start(&mut fx.provider, spec(1));
    assert_eq!(started_on(&ack), Some("a"));
    let listed = fx.registry.hosts();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].id, host("a"));
    assert_eq!(listed[0].lease, Some(lease(1)));
    assert_eq!(listed[0].phase, Phase::Serving);
    assert_eq!(listed[0].labels, map(&[("rack", "r1")]));
    assert_eq!(listed[0].agent_version.as_deref(), Some("0.1.0"));
    assert_eq!(listed[0].epoch, epoch);
    assert_eq!(listed[1].lease, None);
}

/// Asserts that timing is validated by the wire's welcome rules, and that the welcome a
/// host gets carries the pool's interval, grace and name.
#[test]
fn timing_is_validated_and_sent_in_the_welcome() {
    assert!(config(1).with_timing(1_000, 2_000).is_err());
    assert!(config(1).with_timing(0, 10_000).is_err());
    let timed = config(4).with_timing(1_000, 5_000).unwrap();
    let clock = js_lease::ManualClock::default();
    let registry = js_registry::Registry::new(timed, clock);
    let welcome = registry.hello(&hello("a"), &cred("t")).unwrap();
    assert_eq!(
        (
            welcome.heartbeat_interval_ms,
            welcome.grace_ms,
            welcome.pool.as_str()
        ),
        (1_000, 5_000, "builders")
    );
    assert_eq!(welcome.protocol, 1);
    assert_eq!(welcome.epoch, 1);
}

/// Asserts that a credential never prints its digest, and a seat error from the table
/// below passes through untouched.
#[test]
fn credential_debug_hides_the_digest_and_seat_errors_pass_through() {
    assert_eq!(format!("{:?}", cred("secret")), "Credential(..)");
    let fx = fixture(4);
    fx.provider.seat(&host("a"), lease(1), None).unwrap();
    assert_eq!(
        fx.provider.seat(&host("a"), lease(2), None),
        Err(SeatError::Taken {
            host: host("a"),
            holder: lease(1)
        })
    );
}

/// Asserts that every refusal reads as a fixed sentence that repeats nothing the peer
/// sent, so a log line or a reply built from it stays small and cannot be used to echo
/// a hostile string back.
#[test]
fn every_refusal_has_a_fixed_text_without_peer_data() {
    let fx = fixture(1);
    let id = "peer-chosen-id";
    let epoch = fx.join(id, "t");
    let mut refusals = vec![
        fx.registry.hello(&hello(id), &cred("t")).unwrap_err(),
        fx.registry.hello(&hello(id), &cred("u")).unwrap_err(),
        fx.registry
            .hello(&hello("newcomer"), &cred("t"))
            .unwrap_err(),
        fx.registry
            .heartbeat(&heartbeat("nobody", 1), &cred("t"))
            .unwrap_err(),
        fx.registry
            .heartbeat(&heartbeat(id, epoch + 1), &cred("t"))
            .unwrap_err(),
    ];
    let mut elsewhere = hello(id);
    elsewhere.pool = "other".into();
    refusals.push(fx.registry.hello(&elsewhere, &cred("t")).unwrap_err());
    let mut small = hello(id);
    small.shape.cpu_millis = 1;
    fx.pass_grace();
    refusals.push(fx.registry.hello(&small, &cred("t")).unwrap_err());
    let texts: Vec<String> = refusals.iter().map(ToString::to_string).collect();
    assert_eq!(texts.len(), 7);
    for text in &texts {
        assert!(!text.is_empty());
        assert!(!text.contains(id) && !text.contains("newcomer"), "{text}");
    }
    let mut distinct = texts.clone();
    distinct.sort();
    distinct.dedup();
    assert_eq!(distinct.len(), 7);
}

/// Asserts that a host id reserved to another source is refused at hello with the
/// host-listed code, that nothing is created for it (no record, no count against the
/// pool), and that other ids still join. It catches a registered host taking the id of a
/// host the operator lists in a static pool, which would give the table two hosts of one
/// name.
#[test]
fn a_reserved_id_is_refused_at_hello() {
    let fx = fixture(1);
    assert!(fx.registry.reserved().is_empty());
    fx.registry.reserve([host("gpu-01")]);
    fx.registry.reserve([host("gpu-02")]);
    let both = [host("gpu-01"), host("gpu-02")];
    assert_eq!(fx.registry.reserved().into_iter().collect::<Vec<_>>(), both);
    let refused = fx.registry.hello(&hello("gpu-01"), &cred("t")).unwrap_err();
    assert_eq!(refused, Refusal::HostListed);
    assert_eq!(refused.code(), refusal::HOST_LISTED);
    assert!(!refused.to_string().contains("gpu-01"));
    assert!(fx.registry.hosts().is_empty());
    assert!(fx.registry.bindings().is_empty());
    fx.registry.hello(&hello("free"), &cred("t")).unwrap();
}
