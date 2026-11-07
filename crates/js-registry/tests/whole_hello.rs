//! The commit time of a whole-node lease whose agent said hello again.

mod support;

use std::time::Duration;

use js_agent_wire::DEFAULT_GRACE_MS;
use js_provider::{Observed, Pool, StartAck, StartSpec};
use js_registry::{PoolConfig, WholeNodeConfig};
use js_types::{ProviderId, Request, RequestId, ResourceClass};
use support::*;

const TIMEOUT: u64 = 120_000;

fn whole() -> ResourceClass {
    ResourceClass::whole_node()
}

fn request_of(seconds: u64) -> Request {
    Request {
        class: whole().id().clone(),
        max_wall: Duration::from_secs(seconds),
        ..request()
    }
}

fn id() -> RequestId {
    RequestId::new("r").unwrap()
}

fn spec_of(sequence: u64, seconds: u64) -> StartSpec {
    StartSpec::new(lease(sequence), request_of(seconds), &[whole()])
        .unwrap()
        .with_request_id(id())
}

fn recorded(fx: &Fixture) -> bool {
    fx.registry.request_record(&id()).is_some()
}

fn wait(fx: &Fixture, hosts: &[(&str, u64)], ms: u64) {
    let mut left = ms;
    while left > 0 {
        let step = left.min(5_000);
        fx.clock.advance(Duration::from_millis(step));
        hosts.iter().for_each(|(name, epoch)| fx.beat(name, *epoch));
        left -= step;
    }
}

fn pool(names: &[&'static str], engine: Placement) -> (Fixture, Vec<(&'static str, u64)>) {
    let name = ProviderId::new("builders").unwrap();
    let settings = WholeNodeConfig {
        prepare_timeout_ms: TIMEOUT,
        ..WholeNodeConfig::dedicated(4)
    };
    let config = PoolConfig::new(name, whole(), world_shape(), 4)
        .with_whole_node(settings)
        .unwrap();
    let fx = fixture_in(config, engine);
    let hosts = names
        .iter()
        .map(|name| (*name, fx.join_world(name)))
        .collect();
    (fx, hosts)
}

/// A lease in use whose agent restarts and says hello after the lost window gets a whole
/// prepare timeout from the hello: one millisecond short of it the lease is kept and no
/// failure is counted, at it the lease is overdue.
fn a_hello_gives_the_agent_a_whole_prepare_timeout(engine: Placement) {
    let (fx, hosts) = pool(&["a"], engine);
    let mut provider = fx.provider.clone();
    assert_eq!(provider.start(spec_of(1, 600)), StartAck::Unknown);
    let mut ready = fx.heartbeat("a", hosts[0].1);
    ready.exclusive_ready = Some(lease(1).to_string());
    fx.registry.heartbeat(&ready, &cred("t")).unwrap();
    let present = Observed::Present { host: host("a") };
    assert_eq!(provider.observe(&lease(1)), present);
    // In use for longer than a prepare timeout, then the agent goes away and comes back.
    wait(&fx, &hosts, 2 * TIMEOUT);
    fx.pass_grace();
    fx.hello(&world_hello("a"), &cred("t")).unwrap();
    assert_eq!(fx.registry.hosts()[0].lease, Some(lease(1)));
    fx.registry.advance();
    assert_eq!(provider.observe(&lease(1)), Observed::Unknown);
    assert!(!recorded(&fx));
    fx.clock.advance(Duration::from_millis(TIMEOUT - 1));
    fx.registry.advance();
    assert_eq!(provider.observe(&lease(1)), Observed::Unknown);
    assert!(!recorded(&fx));
    fx.clock.advance(Duration::from_millis(1));
    fx.registry.advance();
    assert_eq!(provider.observe(&lease(1)), Observed::Absent);
    let kept = fx.registry.request_record(&id()).unwrap();
    assert_eq!(kept.prepare_failures, 1);
}

/// A hello from a host whose lease is committed but not yet ready leaves the commit time
/// alone, so an agent that keeps restarting cannot postpone the prepare deadline.
fn a_hello_on_a_committed_host_keeps_the_commit_time(engine: Placement) {
    let (fx, _) = pool(&["a"], engine);
    let mut provider = fx.provider.clone();
    assert_eq!(provider.start(spec_of(1, 600)), StartAck::Unknown);
    // The agent goes silent past the grace period and comes back.
    fx.pass_grace();
    let epoch = fx.join_world("a");
    assert_eq!(fx.registry.hosts()[0].lease, Some(lease(1)));
    let gone = DEFAULT_GRACE_MS + 1;
    wait(&fx, &[("a", epoch)], TIMEOUT - gone - 1);
    fx.registry.advance();
    assert_eq!(provider.observe(&lease(1)), Observed::Unknown);
    assert!(!recorded(&fx));
    fx.clock.advance(Duration::from_millis(1));
    fx.registry.advance();
    assert_eq!(provider.observe(&lease(1)), Observed::Absent);
    let kept = fx.registry.request_record(&id()).unwrap();
    assert_eq!(kept.prepare_failures, 1);
}

crate::both!(
    a_hello_gives_the_agent_a_whole_prepare_timeout,
    a_hello_on_a_committed_host_keeps_the_commit_time,
);
