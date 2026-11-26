//! A registry that serves several pools ([`Registry::with_pools`]): a hello joins the pool
//! it names and is checked, booked, timed and counted as that pool's, a host id is one host
//! in one pool, and the first pool is the one the registry is named for.

mod support;

use std::sync::Arc;
use std::time::Duration;

use js_agent_wire::Taint;
use js_registry::{
    Binding, ConfigError, HostState, PoolConfig, Refusal, Registry, SequentialSessions, ShapeFault,
};
use js_types::{ExecMode, ProviderId, Resources, Shape};
use support::*;

/// The second pool: Linux hosts on x86_64 of 16 cores and 64 GiB, in the container mode,
/// timed apart from the first (a 2 s heartbeat, a 10 s grace, a two-minute retention).
fn linux_pool(max_machines: usize) -> PoolConfig {
    let resources = Resources::new(16_000, 64 << 30, 0, None).unwrap();
    let shape = Shape::new(resources, map(&[("os", "linux"), ("arch", "x86_64")]));
    let name = ProviderId::new("farm").unwrap();
    let pool = PoolConfig::new(name, class(), shape, max_machines);
    let pool = pool
        .with_exec(ExecMode::Container)
        .with_timing(2_000, 10_000);
    pool.unwrap().with_lost_retention(120_000).unwrap()
}

/// A registry over the first pool [`config`] (`builders`, macOS on arm64, with a one-minute
/// retention) and [`linux_pool`] (`farm`), each up to `max_machines` hosts.
fn two_pools(max_machines: usize) -> Fixture {
    let first = config(max_machines).with_lost_retention(60_000).unwrap();
    over(first, max_machines)
}

/// A registry over `first` and [`linux_pool`] of up to `max_machines` hosts.
fn over(first: PoolConfig, max_machines: usize) -> Fixture {
    let mut fx = fixture(max_machines);
    let pools = (first, vec![linux_pool(max_machines)]);
    let sessions = SequentialSessions::new();
    let placer = fx.placer.clone();
    fx.registry = Registry::with_pools(pools, fx.clock.clone(), sessions, placer).unwrap();
    fx
}

/// A hello for the pool `farm`: more than its shape, Linux on x86_64, with a container
/// runtime.
fn linux_hello(host: &str) -> js_agent_wire::Hello {
    let mut hello = hello(host);
    hello.pool = "farm".into();
    hello.shape.cpu_millis = 32_000;
    hello.shape.memory_bytes = 128 << 30;
    hello.capabilities = map(&[
        ("os", "linux"),
        ("arch", "x86_64"),
        ("exec", "container"),
        ("runtime", "container"),
    ]);
    hello
}

/// The view of `id` in the registry's listing.
fn view(fx: &Fixture, id: &str) -> js_registry::HostView {
    let hosts = fx.registry.hosts();
    hosts
        .into_iter()
        .find(|view| view.id.as_str() == id)
        .unwrap()
}

/// Each hello joins the pool it names: its welcome is that pool's name and timing, and the
/// host is listed and booked at what it reports with that pool's capabilities. It catches a
/// registry that books every host as the first pool, or answers a host of the second with
/// the first's timing.
#[test]
fn a_hello_joins_the_pool_it_names_and_is_booked_as_that_pool() {
    let fx = two_pools(4);
    let mac = fx.hello(&hello("mac-1"), &cred("m")).unwrap();
    let linux = fx.hello(&linux_hello("node-1"), &cred("l")).unwrap();
    assert_eq!(mac.pool, "builders");
    assert_eq!(linux.pool, "farm");
    assert_eq!(linux.heartbeat_interval_ms, 2_000);
    assert_eq!(linux.grace_ms, 10_000);
    assert_eq!(mac.grace_ms, js_agent_wire::DEFAULT_GRACE_MS);
    let mac_shape = view(&fx, "mac-1").shape;
    assert_eq!(mac_shape.resources().cpu_millis(), 8_000);
    assert_eq!(mac_shape.resources().memory_bytes(), 16 << 30);
    assert_eq!(mac_shape.capabilities(), pool_shape().capabilities());
    let farm = view(&fx, "node-1").shape;
    assert_eq!(farm.resources().cpu_millis(), 32_000);
    assert_eq!(farm.resources().memory_bytes(), 128 << 30);
    assert_eq!(farm.capabilities()["os"], "linux");
    assert_eq!(farm.capabilities()["exec"], "container");
    assert_eq!(fx.placer.hosts_kept(), 2);
}

/// A hello that names a pool the registry does not serve is refused as an unknown pool and
/// creates nothing, a name that differs from a pool's in case included. It catches a
/// registry that falls back to its first pool for a name it does not hold.
#[test]
fn a_hello_for_a_pool_the_registry_does_not_serve_is_refused() {
    let fx = two_pools(4);
    for pool in ["gpus", "Farm", "builders-2"] {
        let mut other = hello("h");
        other.pool = pool.into();
        let refused = fx.registry.hello(&other, &cred("t"));
        assert_eq!(refused.unwrap_err(), Refusal::UnknownPool, "{pool:?}");
    }
    assert!(fx.registry.hosts().is_empty());
}

/// A hello is held to the shape and the execution mode of the pool it names, not to those
/// of another pool it would meet: a Mac in the Linux pool is below its floor, a Linux host
/// in the Mac pool lacks its `os`, and a host of the Linux pool that reports another mode
/// breaks its contract. It catches the floor or the contract read from the first pool.
#[test]
fn a_hello_is_checked_against_the_floor_and_the_mode_of_its_own_pool() {
    let fx = two_pools(4);
    let mut mac_in_farm = hello("mac-1");
    mac_in_farm.pool = "farm".into();
    mac_in_farm
        .capabilities
        .extend(map(&[("runtime", "container")]));
    let refused = fx.registry.hello(&mac_in_farm, &cred("t")).unwrap_err();
    assert_eq!(refused, Refusal::ShapeNotAccepted(ShapeFault::BelowFloor));
    let mut linux_in_builders = linux_hello("node-1");
    linux_in_builders.pool = "builders".into();
    let refused = fx
        .registry
        .hello(&linux_in_builders, &cred("t"))
        .unwrap_err();
    assert_eq!(refused, Refusal::ShapeNotAccepted(ShapeFault::BelowFloor));
    let mut native = linux_hello("node-2");
    native
        .capabilities
        .insert("exec".into(), "sandbox-exec".into());
    let refused = fx.registry.hello(&native, &cred("t")).unwrap_err();
    assert_eq!(refused.code(), js_agent_wire::refusal::EXECUTION_MISMATCH);
    assert!(fx.registry.hosts().is_empty());
}

/// A host id is one host in one pool: once a host is in one pool, a hello for its id in
/// the other is refused as host-listed, even after it is lost, and its record stays as it
/// was, still booked at what its first hello reported. It catches a host moved into another
/// pool's shape by a hello that names that pool.
#[test]
fn a_host_of_one_pool_cannot_say_hello_in_another() {
    let fx = two_pools(4);
    fx.hello(&hello("h"), &cred("t")).unwrap();
    fx.pass_grace();
    let mut moved = linux_hello("h");
    moved.shape = hello("h").shape;
    moved.shape.cpu_millis = 32_000;
    moved.shape.memory_bytes = 128 << 30;
    let refused = fx.registry.hello(&moved, &cred("t")).unwrap_err();
    assert_eq!(refused, Refusal::HostListed);
    let kept = view(&fx, "h");
    assert_eq!(kept.shape.resources().cpu_millis(), 8_000);
    assert_eq!(kept.shape.resources().memory_bytes(), 16 << 30);
    assert_eq!(kept.shape.capabilities(), pool_shape().capabilities());
    assert_eq!(kept.state, HostState::Lost);
    assert_eq!(kept.epoch, 1);
    assert_eq!(fx.hello(&hello("h"), &cred("t")).unwrap().epoch, 2);
}

/// Each pool counts its own hosts against its own machine limit: a full pool refuses a new
/// host while the other still admits one, and the hosts of one pool never fill the other.
#[test]
fn each_pool_counts_its_own_hosts_against_its_limit() {
    let fx = two_pools(1);
    fx.hello(&hello("mac-1"), &cred("t")).unwrap();
    fx.hello(&linux_hello("node-1"), &cred("t")).unwrap();
    let full = fx.registry.hello(&hello("mac-2"), &cred("t")).unwrap_err();
    assert_eq!(full, Refusal::PoolFull);
    let full = fx.registry.hello(&linux_hello("node-2"), &cred("t"));
    assert_eq!(full.unwrap_err(), Refusal::PoolFull);
    assert_eq!(fx.registry.hosts().len(), 2);
}

/// A lost host is timed by its own pool: past the Linux pool's grace (10 s) and before the
/// Mac pool's (15 s) only the Linux host is lost; past the Linux pool's grace and two-minute
/// retention, and the Mac pool's grace and one-minute retention but not the Linux one, only
/// the Mac host is forgotten. It catches every host timed by the first pool.
#[test]
fn a_host_is_lost_and_forgotten_by_the_timing_of_its_own_pool() {
    let fx = two_pools(4);
    fx.hello(&hello("mac-1"), &cred("t")).unwrap();
    fx.hello(&linux_hello("node-1"), &cred("t")).unwrap();
    fx.clock.advance(Duration::from_millis(10_001));
    assert_eq!(view(&fx, "node-1").state, HostState::Lost);
    assert_eq!(view(&fx, "mac-1").state, HostState::Live);
    let since = fx.registry.silent_since(&host("node-1"));
    assert_eq!(since.map(|at| at.millis()), Some(1_000_000 + 10_000));
    assert_eq!(fx.registry.silent_since(&host("mac-1")), None);
    // The Mac host is past its grace (15 s) and retention (60 s); the Linux host is not
    // past its grace (10 s) and retention (120 s).
    fx.clock.advance(Duration::from_millis(70_000));
    let left: Vec<String> = fx
        .registry
        .hosts()
        .iter()
        .map(|view| view.id.to_string())
        .collect();
    assert_eq!(left, ["node-1"]);
    assert_eq!(fx.placer.hosts_kept(), 1);
    fx.clock.advance(Duration::from_millis(60_000));
    assert!(fx.registry.hosts().is_empty());
}

/// A heartbeat check answers as the housekeeping would by the host's own pool: a Linux host
/// past its pool's grace and two-minute retention is unknown to the check, while a Mac host
/// heard as long ago, inside its pool's ten-minute retention, is still answered.
#[test]
fn the_heartbeat_check_forgets_a_host_by_its_own_pools_timing() {
    let long = config(4).with_lost_retention(600_000).unwrap();
    let fx = over(long, 4);
    fx.hello(&hello("mac-1"), &cred("t")).unwrap();
    fx.hello(&linux_hello("node-1"), &cred("t")).unwrap();
    fx.clock.advance(Duration::from_millis(130_001));
    let linux = fx
        .registry
        .check_heartbeat(&fx.heartbeat("node-1", 1), &cred("t"));
    assert_eq!(linux.unwrap_err(), Refusal::UnknownHost);
    let mac = fx
        .registry
        .check_heartbeat(&fx.heartbeat("mac-1", 1), &cred("t"));
    assert!(mac.is_ok());
}

/// A host put back after a restart (a binding names no pool) is in no pool until it says
/// hello, so its hello may join either pool and is booked as the pool it names; it counts
/// toward the first pool's limit until then. It catches a restored host held to the first
/// pool, which would lock a tainted Mac out of its own pool after every restart.
#[test]
fn a_restored_host_joins_the_pool_its_next_hello_names() {
    let fx = two_pools(1);
    let taint = Taint {
        reason: "kept".into(),
        run_id: None,
        check: None,
    };
    let bound = Binding::new(cred("t"), None).with_taint(Some(taint));
    fx.registry.restore_tainted(&host("node-1"), bound);
    assert_eq!(view(&fx, "node-1").state, HostState::Awaiting);
    let full = fx.registry.hello(&hello("mac-1"), &cred("t")).unwrap_err();
    assert_eq!(full, Refusal::PoolFull);
    let back = fx.hello(&linux_hello("node-1"), &cred("t")).unwrap();
    assert_eq!(back.pool, "farm");
    assert_eq!(view(&fx, "node-1").shape.capabilities()["os"], "linux");
    assert!(view(&fx, "node-1").taint.is_some());
    assert_eq!(fx.join("mac-1", "t"), 1);
}

/// Two pools of one name are refused with a message that names it: a hello could not say
/// which it joins. Pools of different names are taken, and a registry of one pool answers
/// with that pool's name.
#[test]
fn two_pools_of_one_name_are_refused() {
    let fx = fixture(4);
    let placer = fx.placer.clone();
    let twice = (config(4), vec![linux_pool(4), config(2)]);
    let refused = Registry::with_pools(twice, fx.clock.clone(), SequentialSessions::new(), placer);
    let refused = refused.err().unwrap();
    assert_eq!(refused, ConfigError::PoolNamedTwice("builders".into()));
    assert_eq!(
        refused.to_string(),
        "two pools are named \"builders\": a hello could not say which it joins"
    );
}

/// The registry is named for its first pool and has the first pool's whole-node settings:
/// a second pool's do not replace them, and a registry whose first pool has none has none.
#[test]
fn the_first_pool_names_the_registry_and_gives_its_whole_node_settings() {
    let fx = two_pools(4);
    assert_eq!(fx.registry.pool_id().as_str(), "builders");
    assert!(fx.registry.whole_node().is_none());
    let whole = js_registry::WholeNodeConfig::dedicated(4);
    let farm = linux_pool(4);
    let whole_class = js_types::ResourceClass::whole_node();
    let name = ProviderId::new("farm").unwrap();
    let farm = PoolConfig::new(name, whole_class, farm.shape().clone(), 4);
    let farm = farm.with_whole_node(whole).unwrap();
    let pools = (farm, vec![config(4)]);
    let placer: Arc<dyn js_registry::Placer> = fx.placer.clone();
    let sessions = SequentialSessions::new();
    let registry = Registry::with_pools(pools, fx.clock.clone(), sessions, placer).unwrap();
    assert_eq!(registry.pool_id().as_str(), "farm");
    assert!(registry.whole_node().is_some());
}

/// Asserts that a registry of several pools gives the shortest of their heartbeat intervals,
/// whichever pool has it: 2 s for the default 5 s pool with the 2 s `farm` beside it, and 2 s
/// with the two the other way round. It catches a server that holds a heartbeat's reply for
/// the first pool's interval, longer than the interval a host of another pool beats at.
#[test]
fn the_heartbeat_interval_of_several_pools_is_the_shortest() {
    let fx = two_pools(4);
    assert_eq!(fx.registry.heartbeat_interval(), Duration::from_secs(2));
    let pools = (linux_pool(4), vec![config(4)]);
    let placer: Arc<dyn js_registry::Placer> = fx.placer.clone();
    let sessions = SequentialSessions::new();
    let registry = Registry::with_pools(pools, fx.clock.clone(), sessions, placer).unwrap();
    assert_eq!(registry.heartbeat_interval(), Duration::from_secs(2));
}
