//! What a host id is bound to, what a restart does to a seated lease's binding, and when
//! a lost host is forgotten.

mod support;

use std::time::Duration;

use js_agent_wire::{DEFAULT_GRACE_MS, InstanceIdentity, refusal};
use js_lease::ManualClock;
use js_provider::{Observed, Provider, RefusalCode, StartAck};
use js_registry::{
    Binding, ConfigError, DEFAULT_LOST_RETENTION_MS, HostState, MAX_LOST_RETENTION_MS,
    MIN_LOST_RETENTION_MS, Refusal, Registry, SequentialSessions, SessionSource,
};
use support::*;

const RETENTION_MS: u64 = MIN_LOST_RETENTION_MS;

fn instance(id: &str) -> InstanceIdentity {
    InstanceIdentity {
        provider: "aws".into(),
        instance_id: id.into(),
    }
}

fn on(host: &str, id: &str) -> js_agent_wire::Hello {
    let mut hello = hello(host);
    hello.instance = Some(instance(id));
    hello
}

/// A fixture whose pool forgets a lost, unbooked host after one minute.
fn short_retention(max_machines: usize) -> Fixture {
    let config = config(max_machines)
        .with_lost_retention(RETENTION_MS)
        .unwrap();
    fixture_of(config)
}

fn advance(fx: &Fixture, millis: u64) {
    fx.clock.advance(Duration::from_millis(millis));
}

/// Lets a host that last spoke now be lost for exactly the retention: still kept.
fn lost_for_retention(fx: &Fixture) {
    advance(fx, DEFAULT_GRACE_MS + RETENTION_MS);
}

/// Asserts that a host id first admitted with an instance identity refuses a hello with
/// the same token and another instance, or none: a holder of the pool's token cannot take
/// over a lost host that was bound to its machine, and so cannot inherit its booking. The
/// same instance gets back in. Without the instance part of the binding the first two
/// hellos are admitted and the lease reads Present on the intruder.
#[test]
fn instance_bound_host_refuses_another_instance_with_the_same_token() {
    let mut fx = fixture(4);
    fx.registry.hello(&on("a", "i-1"), &cred("t")).unwrap();
    fx.provider.start(spec(1));
    fx.pass_grace();
    let other = fx.registry.hello(&on("a", "i-2"), &cred("t")).unwrap_err();
    assert_eq!(other, Refusal::HostBound);
    assert_eq!(other.code(), refusal::HOST_BOUND);
    let none = fx.registry.hello(&hello("a"), &cred("t")).unwrap_err();
    assert_eq!(none, Refusal::HostBound);
    let mut other_provider = on("a", "i-1");
    other_provider.instance.as_mut().unwrap().provider = "gcp".into();
    let elsewhere = fx.registry.hello(&other_provider, &cred("t")).unwrap_err();
    assert_eq!(elsewhere, Refusal::HostBound);
    assert_eq!(fx.provider.observe(&lease(1)), Observed::Unknown);
    let welcome = fx.registry.hello(&on("a", "i-1"), &cred("t")).unwrap();
    assert_eq!(welcome.epoch, 2);
    assert_eq!(
        fx.provider.observe(&lease(1)),
        Observed::Present { host: host("a") }
    );
}

/// Asserts the stated limit of the binding, so a change to it is a decision: a host id
/// first admitted without an instance identity is bound to the pool credential only, and
/// a hello for it with the same token takes it over once it is lost, with its booking,
/// whatever instance it names. The binding is not upgraded by that hello: the first
/// holder of the token without an instance still gets back in.
#[test]
fn host_without_instance_is_bound_to_the_pool_credential_only() {
    let mut fx = fixture(4);
    fx.join("a", "t");
    fx.provider.start(spec(1));
    fx.pass_grace();
    let welcome = fx.registry.hello(&on("a", "i-9"), &cred("t")).unwrap();
    assert_eq!(welcome.epoch, 2);
    assert_eq!(
        fx.provider.observe(&lease(1)),
        Observed::Present { host: host("a") }
    );
    fx.pass_grace();
    assert_eq!(fx.join("a", "t"), 3);
    fx.pass_grace();
    let other_token = fx.registry.hello(&on("a", "i-9"), &cred("u")).unwrap_err();
    assert_eq!(other_token, Refusal::HostBound);
}

/// Asserts that a lease seated with a binding is held for that binding: the awaiting host
/// refuses a hello from another credential, and from another instance with the right
/// credential, and stays awaiting with its lease unobservable; the bound hello takes it
/// and the lease is Present. Without the binding on the awaiting record any holder of
/// the pool's token takes the id and the restored lease.
#[test]
fn seat_with_a_binding_refuses_a_hello_that_does_not_match() {
    let mut fx = fixture(4);
    let binding = Binding::new(cred("t"), Some(instance("i-1")));
    fx.provider
        .seat(&host("a"), lease(1), Some(binding))
        .unwrap();
    let token = fx.registry.hello(&on("a", "i-1"), &cred("u")).unwrap_err();
    assert_eq!(token, Refusal::HostBound);
    let machine = fx.registry.hello(&on("a", "i-2"), &cred("t")).unwrap_err();
    assert_eq!(machine, Refusal::HostBound);
    let bare = fx.registry.hello(&hello("a"), &cred("t")).unwrap_err();
    assert_eq!(bare, Refusal::HostBound);
    assert_eq!(fx.registry.hosts()[0].state, HostState::Awaiting);
    assert_eq!(fx.provider.observe(&lease(1)), Observed::Unknown);
    assert_eq!(
        fx.registry
            .hello(&on("a", "i-1"), &cred("t"))
            .unwrap()
            .epoch,
        1
    );
    assert_eq!(
        fx.provider.observe(&lease(1)),
        Observed::Present { host: host("a") }
    );
}

/// Asserts that a binding seated with a credential and no instance holds the credential
/// only: the right token with any instance is admitted. The restart gap, as documented:
/// with no binding at all (`None`), the first hello from any token holder takes the id
/// and the lease, and is the binding from then on, instance included.
#[test]
fn seat_without_a_binding_lets_the_first_hello_bind() {
    let fx = fixture(4);
    let credential_only = Binding::new(cred("t"), None);
    fx.provider
        .seat(&host("a"), lease(1), Some(credential_only))
        .unwrap();
    assert_eq!(
        fx.registry.hello(&hello("a"), &cred("u")).unwrap_err(),
        Refusal::HostBound
    );
    fx.registry.hello(&on("a", "i-5"), &cred("t")).unwrap();
    fx.provider.seat(&host("b"), lease(2), None).unwrap();
    fx.registry.hello(&on("b", "i-1"), &cred("anyone")).unwrap();
    fx.pass_grace();
    let other_instance = fx.registry.hello(&on("b", "i-2"), &cred("anyone"));
    assert_eq!(other_instance.unwrap_err(), Refusal::HostBound);
    let other_token = fx.registry.hello(&on("b", "i-1"), &cred("else"));
    assert_eq!(other_token.unwrap_err(), Refusal::HostBound);
    assert_eq!(
        fx.registry
            .hello(&on("b", "i-1"), &cred("anyone"))
            .unwrap()
            .epoch,
        2
    );
}

/// Asserts that ids a token holder made up and abandoned stop blocking real hosts once
/// they have been lost for the retention: with the pool full of lost squatters a real
/// host is refused as pool-full until the retention has passed, exactly at the
/// retention they are kept, and one millisecond later they are gone and the real host
/// joins. Without the sweep the pool stays full for ever.
#[test]
fn squatting_ids_stop_blocking_real_hosts_after_the_retention() {
    let fx = short_retention(2);
    fx.join("squat-1", "t");
    fx.join("squat-2", "t");
    lost_for_retention(&fx);
    let full = fx.registry.hello(&hello("real"), &cred("t")).unwrap_err();
    assert_eq!(full, Refusal::PoolFull);
    assert_eq!(fx.registry.hosts().len(), 2);
    advance(&fx, 1);
    assert_eq!(fx.join("real", "t"), 1);
    let listed = fx.registry.hosts();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, host("real"));
    assert_eq!(fx.provider.capacity().len(), 1);
}

/// Asserts that a lost host holding a booking is never forgotten, however long it is
/// silent, while an unbooked one beside it is; that it comes back with its booking; and
/// that once the lease is stopped the host is forgotten at the next call, since it is
/// lost and holds nothing. Without the booking exemption a long outage would drop the
/// lease's host, and the lease with it.
#[test]
fn a_booked_lost_host_is_kept_until_its_lease_stops() {
    let mut fx = short_retention(4);
    fx.join("a", "t");
    fx.join("b", "t");
    assert_eq!(started_on(&fx.provider.start(spec(1))), Some("a"));
    lost_for_retention(&fx);
    advance(&fx, 1);
    let listed = fx.registry.hosts();
    assert_eq!(listed.len(), 1);
    assert_eq!(
        (listed[0].id.clone(), listed[0].state, listed[0].lease),
        (host("a"), HostState::Lost, Some(lease(1)))
    );
    advance(&fx, MIN_LOST_RETENTION_MS * 1000);
    assert_eq!(fx.provider.observe(&lease(1)), Observed::Unknown);
    assert_eq!(fx.registry.hello(&hello("a"), &cred("t")).unwrap().epoch, 2);
    fx.pass_grace();
    advance(&fx, RETENTION_MS);
    assert!(matches!(
        fx.provider.stop(&lease(1)),
        js_provider::StopAck::Stopped { .. }
    ));
    assert!(fx.registry.hosts().is_empty());
    assert!(fx.provider.capacity().is_empty());
    assert_eq!(
        fx.provider.start(spec(2)),
        StartAck::Refused(RefusalCode::NoCapacity)
    );
}

/// Asserts that a host bound to a token that was rotated away is refused while its
/// record is kept, and registers again with the new token once the record is forgotten,
/// as a new host with epoch 1; a heartbeat for the forgotten session is refused as
/// unknown host. Without the binding dropping with the record, the id is locked out for
/// good.
#[test]
fn a_host_bound_to_a_rotated_token_registers_again_after_eviction() {
    let fx = short_retention(4);
    fx.join("a", "old-token");
    fx.pass_grace();
    let refused = fx.registry.hello(&hello("a"), &cred("new-token"));
    assert_eq!(refused.unwrap_err(), Refusal::HostBound);
    advance(&fx, RETENTION_MS);
    let beat = fx
        .registry
        .heartbeat(&heartbeat("a", 1), &cred("old-token"));
    assert_eq!(beat.unwrap_err(), Refusal::UnknownHost);
    let welcome = fx.registry.hello(&hello("a"), &cred("new-token")).unwrap();
    assert_eq!(welcome.epoch, 1);
}

/// Asserts that an awaiting host, which has never been heard and so has no time to count
/// from, is not forgotten however much time passes.
#[test]
fn an_awaiting_host_is_not_forgotten() {
    let fx = short_retention(4);
    fx.provider.seat(&host("a"), lease(1), None).unwrap();
    advance(&fx, MAX_LOST_RETENTION_MS * 2);
    assert_eq!(fx.registry.hosts()[0].state, HostState::Awaiting);
}

/// Asserts that the retention is bounded: a minute to thirty days are accepted and
/// anything outside is refused with the config error, and the default is inside the
/// bounds and is what a pool keeps lost hosts for when the operator says nothing.
#[test]
fn lost_retention_is_validated() {
    assert!(config(1).with_lost_retention(MIN_LOST_RETENTION_MS).is_ok());
    assert!(config(1).with_lost_retention(MAX_LOST_RETENTION_MS).is_ok());
    for bad in [0, MIN_LOST_RETENTION_MS - 1, MAX_LOST_RETENTION_MS + 1] {
        let refused = config(1).with_lost_retention(bad).unwrap_err();
        assert_eq!(refused, ConfigError::LostRetention);
        assert!(refused.to_string().contains("retention"));
    }
    const { assert!(MIN_LOST_RETENTION_MS <= DEFAULT_LOST_RETENTION_MS) };
    const { assert!(DEFAULT_LOST_RETENTION_MS <= MAX_LOST_RETENTION_MS) };
    let fx = fixture(1);
    fx.join("a", "t");
    advance(&fx, DEFAULT_GRACE_MS + DEFAULT_LOST_RETENTION_MS);
    assert_eq!(fx.registry.hosts().len(), 1);
    advance(&fx, 1);
    assert!(fx.registry.hosts().is_empty());
}

/// Asserts that a peer holding the same pool token, and knowing the host id and the
/// epoch (both guessable), cannot heartbeat for an instance-bound host: with no session
/// secret, an empty one or a guessed one it is refused as `session-mismatch`, and the
/// host's state, phase, epoch and last-heard time are exactly as before, so the dead
/// host stays lost and takes no lease. The session the host was given at its hello still
/// works and revives it. Without the session check the peer's first heartbeat is
/// accepted: it keeps a dead host live and bookable, so leases land on a dead machine,
/// and sets the host's phase.
#[test]
fn heartbeat_from_a_peer_with_the_same_token_but_no_session_is_refused_and_changes_nothing() {
    let mut fx = fixture(4);
    let welcome = fx.hello(&on("a", "i-1"), &cred("pool")).unwrap();
    fx.pass_grace();
    let before = fx.registry.hosts();
    assert_eq!(before[0].state, HostState::Lost);
    let mut peer = heartbeat("a", welcome.epoch);
    peer.phase = js_agent_wire::Phase::Draining;
    let same_length = "0".repeat(welcome.session.len());
    for guess in ["", "guess", same_length.as_str()] {
        peer.session = guess.into();
        let refused = fx.registry.heartbeat(&peer, &cred("pool")).unwrap_err();
        assert_eq!(refused, Refusal::SessionMismatch);
        assert_eq!(refused.code(), refusal::SESSION_MISMATCH);
        assert_eq!(fx.registry.hosts(), before);
    }
    assert_eq!(
        fx.provider.start(spec(1)),
        StartAck::Refused(RefusalCode::NoCapacity)
    );
    let mut real = fx.heartbeat("a", welcome.epoch);
    real.phase = js_agent_wire::Phase::Draining;
    fx.registry.heartbeat(&real, &cred("pool")).unwrap();
    let view = &fx.registry.hosts()[0];
    assert_eq!(view.state, HostState::Live);
    assert_eq!(view.phase, js_agent_wire::Phase::Draining);
    assert_eq!(view.epoch, welcome.epoch);
    assert_eq!(
        fx.registry.heartbeat(&real, &cred("outside")).unwrap_err(),
        Refusal::HostBound
    );
}

/// Asserts that a hello replaces the session: after a host says hello again (once it is
/// lost) a heartbeat carrying the old session is refused as `session-mismatch` even with
/// the new epoch, the old session with the old epoch is a stale epoch, and the session
/// of the new hello works. It catches a session that survives a hello, which would let
/// the holder of an earlier secret (a previous run of the agent, or a copy of its
/// memory) keep speaking for the new session.
#[test]
fn a_new_hello_invalidates_the_old_session() {
    let fx = fixture(4);
    let first = fx.hello(&hello("a"), &cred("t")).unwrap();
    fx.pass_grace();
    let second = fx.hello(&hello("a"), &cred("t")).unwrap();
    assert_eq!(second.epoch, first.epoch + 1);
    assert_ne!(second.session, first.session);
    let mut old = heartbeat("a", second.epoch);
    old.session = first.session.clone();
    let refused = fx.registry.heartbeat(&old, &cred("t")).unwrap_err();
    assert_eq!(refused, Refusal::SessionMismatch);
    old.epoch = first.epoch;
    let stale = fx.registry.heartbeat(&old, &cred("t")).unwrap_err();
    assert_eq!(stale, Refusal::StaleEpoch);
    fx.beat("a", second.epoch);
    assert_eq!(fx.registry.hosts()[0].state, HostState::Live);
}

/// Asserts the OS source issues a fresh, well-formed secret at every hello: two hosts
/// get different ones, each inside the wire's session alphabet and cap (the agent's
/// welcome check accepts it). It catches a constant or empty secret from the production source.
#[test]
fn the_os_source_issues_distinct_wire_valid_secrets() {
    let registry = Registry::new(config(4), ManualClock::default());
    let a = registry.hello(&hello("a"), &cred("t")).unwrap();
    let b = registry.hello(&hello("b"), &cred("t")).unwrap();
    assert_ne!(a.session, b.session);
    for welcome in [&a, &b] {
        assert_eq!(welcome.validate(), Ok(()));
        assert_eq!(welcome.session.len(), 64);
    }
}

/// A source that can make no bytes, as when the operating system's random source fails.
struct NoRandomness;

impl SessionSource for NoRandomness {
    fn fresh(&self) -> Option<[u8; 32]> {
        None
    }
}

/// Asserts that when no session secret can be made the hello is refused as
/// `session-unavailable` and the host is not admitted: nothing is created. It catches
/// admitting a host with a guessable (zeroed or empty) secret on a failed random source.
#[test]
fn a_hello_is_refused_when_no_secret_can_be_made() {
    let registry = Registry::with_sessions(config(4), ManualClock::default(), NoRandomness);
    let refused = registry.hello(&hello("a"), &cred("t")).unwrap_err();
    assert_eq!(refused, Refusal::SessionUnavailable);
    assert_eq!(refused.code(), refusal::SESSION_UNAVAILABLE);
    assert!(registry.hosts().is_empty());
}

/// Asserts the deterministic source counts from 1. It catches a test source that
/// repeats, which would hide a session that survives a hello.
#[test]
fn the_sequential_source_counts_from_one() {
    let source = SequentialSessions::new();
    let first = source.fresh().unwrap();
    let second = source.fresh().unwrap();
    assert_eq!((first[31], second[31]), (1, 2));
    assert_eq!(first[..24], [0; 24]);
}

/// Asserts that `bindings` lists what each known host id is bound to (the credential and
/// the instance of its first hello), that a seated booking shows the binding it was
/// seated with, and that a seat without one shows none until the first hello binds the
/// id. It catches a caller that persists bindings getting a list that is empty, that
/// leaves out an awaiting host's binding (so a second restart would lose it), or that
/// invents one.
#[test]
fn bindings_list_what_each_host_is_bound_to() {
    let fx = fixture(4);
    assert!(fx.registry.bindings().is_empty());
    fx.registry.hello(&on("a", "i-1"), &cred("t")).unwrap();
    fx.registry.hello(&hello("b"), &cred("u")).unwrap();
    let listed = fx.registry.bindings();
    let a = &listed[&host("a")];
    assert_eq!(a.credential(), &cred("t"));
    assert_eq!(a.instance(), Some(&instance("i-1")));
    let b = &listed[&host("b")];
    assert_eq!(b.credential(), &cred("u"));
    assert_eq!(b.instance(), None);

    let restarted = fixture(4);
    let kept = Binding::new(cred("t"), Some(instance("i-9")));
    restarted
        .provider
        .seat(&host("c"), lease(1), Some(kept.clone()))
        .unwrap();
    restarted.provider.seat(&host("d"), lease(2), None).unwrap();
    let awaiting = restarted.registry.bindings();
    assert_eq!(awaiting.len(), 1);
    assert_eq!(awaiting[&host("c")], kept);
}
