//! What a server needs of the registry to keep the hosts' `node/` rows in the job store: the
//! times that judge a row live, refresh it and forget it, a pool's machine cap and its booking
//! rule, the digest of a host's session, and the withdrawal of a first hello the rows refused.

mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use js_agent_wire::DEFAULT_GRACE_MS;
use js_registry::{DEFAULT_LOST_RETENTION_MS, Placer, PoolConfig, Registry, SequentialSessions};
use js_types::{HostId, ProviderId, Resources, Shape};
use support::*;

/// The second pool: a 2 s heartbeat, a 10 s grace, a two-minute retention, up to five hosts,
/// placing each host by its hello.
fn farm() -> PoolConfig {
    let resources = Resources::new(16_000, 64 << 30, 0, None).unwrap();
    let shape = Shape::new(resources, map(&[("os", "linux"), ("arch", "x86_64")]));
    let name = ProviderId::new("farm").unwrap();
    let pool = PoolConfig::new(name, class(), shape, 5).placing_by_hello();
    let pool = pool.with_timing(2_000, 10_000).unwrap();
    pool.with_lost_retention(120_000).unwrap()
}

/// A registry over [`config`] of three hosts (`builders`, retention one minute) and [`farm`].
fn two_pools() -> Fixture {
    let first = config(3).with_lost_retention(60_000).unwrap();
    let mut fx = fixture(3);
    let pools = (first, vec![farm()]);
    let sessions = SequentialSessions::new();
    let placer = fx.placer.clone();
    fx.registry = Registry::with_pools(pools, fx.clock.clone(), sessions, placer).unwrap();
    fx
}

/// A placer that writes down the hosts it is told are gone.
#[derive(Default)]
struct Gone(Mutex<Vec<String>>);

impl Placer for Gone {
    fn host_gone(&self, host: &HostId) {
        self.0.lock().unwrap().push(host.to_string());
    }
}

fn host(name: &str) -> HostId {
    HostId::new(name).unwrap()
}

/// Asserts the times of a registry over one pool: the grace it serves, half of it to refresh a
/// row, and the grace plus the default lost-host retention to forget one. It catches a row
/// judged live by another grace than the registry's, and a forget sooner than the registry
/// forgets the host.
#[test]
fn one_pools_times_are_its_grace_half_of_it_and_the_grace_plus_retention() {
    let fx = fixture(3);
    let grace = Duration::from_millis(DEFAULT_GRACE_MS);
    assert_eq!(fx.registry.longest_grace(), grace);
    assert_eq!(fx.registry.refresh_every(), grace / 2);
    let retention = Duration::from_millis(DEFAULT_LOST_RETENTION_MS);
    assert_eq!(fx.registry.forget_after(), grace + retention);
}

/// Asserts the times of a registry over two pools: a row is live for the longest grace, is
/// refreshed at half the shortest, and is forgotten after the longest grace plus the longest
/// retention. It catches a row of the pool with the longer grace judged lost by the shorter
/// one's, and a row forgotten while the longer retention still keeps its host.
#[test]
fn several_pools_times_are_the_longest_grace_the_shortest_halved_and_the_longest_retention() {
    let fx = two_pools();
    let longest = Duration::from_millis(DEFAULT_GRACE_MS);
    assert_eq!(fx.registry.longest_grace(), longest);
    assert_eq!(fx.registry.refresh_every(), Duration::from_secs(5));
    let retention = Duration::from_secs(120);
    assert_eq!(fx.registry.forget_after(), longest + retention);
}

/// Asserts that a pool's cap is its machine count, per pool, and that a pool the registry does
/// not serve has none. It catches a cap read from the wrong pool, and a cap invented for a pool
/// that is not served.
#[test]
fn a_pools_cap_is_its_machine_count_and_a_pool_not_served_has_none() {
    let fx = two_pools();
    assert_eq!(fx.registry.pool_cap("builders"), Some(3));
    assert_eq!(fx.registry.pool_cap("farm"), Some(5));
    assert_eq!(fx.registry.pool_cap("elsewhere"), None);
}

/// Asserts the shape a job is placed against on a host of a pool: a pool that places by hello
/// adds the capabilities the host claimed beyond the pool's own, a pool that does not keeps its
/// own, and a pool not served has no shape. It catches a claim that is ignored in a pool that
/// places by hello, and one that leaks into a pool that does not.
#[test]
fn a_hosts_placement_shape_follows_its_pools_booking_rule() {
    let fx = two_pools();
    let resources = Resources::new(4000, 8 << 30, 0, None).unwrap();
    let booked = Shape::new(resources.clone(), map(&[("os", "linux")]));
    let claim = Shape::new(
        resources,
        map(&[("os", "linux"), ("layered-rootfs", "true")]),
    );

    let by_hello = fx
        .registry
        .placement_shape_in("farm", &booked, Some(&claim))
        .unwrap();
    assert_eq!(
        by_hello.capabilities().get("layered-rootfs").unwrap(),
        "true"
    );
    let own = fx
        .registry
        .placement_shape_in("builders", &booked, Some(&claim))
        .unwrap();
    assert_eq!(own.capabilities(), booked.capabilities());
    let no_claim = fx.registry.placement_shape_in("farm", &booked, None);
    assert_eq!(no_claim.unwrap().capabilities(), booked.capabilities());
    assert_eq!(
        fx.registry
            .placement_shape_in("elsewhere", &booked, Some(&claim)),
        None
    );
}

/// Asserts that the digest of a host's session is there once it said hello, is the same until
/// the next hello, differs between hosts and after a new hello, and is absent for a host the
/// registry does not hold. It catches a digest that is shared between hosts or does not follow
/// the session.
#[test]
fn a_hosts_session_digest_follows_its_hello() {
    let fx = fixture(3);
    fx.join("a", "t");
    fx.join("b", "t");
    let first = fx.registry.session_digest(&host("a")).unwrap();
    assert_eq!(fx.registry.session_digest(&host("a")), Some(first));
    assert_ne!(fx.registry.session_digest(&host("b")), Some(first));
    fx.pass_grace();
    fx.join("a", "t");
    assert_ne!(fx.registry.session_digest(&host("a")), Some(first));
    assert_eq!(fx.registry.session_digest(&host("c")), None);
}

/// Asserts that a first hello taken back leaves no trace: the registry no longer holds the
/// host, the placer is told it is gone, and its place in the pool is free for another host. It
/// catches a withdrawal that leaves a host the rows refused counted against the pool.
#[test]
fn a_first_hello_taken_back_frees_its_place_and_tells_the_placer() {
    let told = Arc::new(Gone::default());
    let fx = with_placer(config(1), told.clone());
    fx.join("a", "t");
    assert!(fx.registry.withdraw_first_hello(&host("a"), 1));
    assert!(fx.registry.own_host(&host("a")).is_none());
    assert_eq!(*told.0.lock().unwrap(), ["a"]);
    fx.join("b", "t");
    assert!(fx.registry.own_host(&host("b")).is_some());
}

/// Asserts that only a host's first hello is taken back: a host the registry does not hold, a
/// hello of a later epoch, and a first-epoch withdrawal naming a host already at a later epoch
/// leave the host as it is. It catches a withdrawal that removes a host that was live before
/// the hello it takes back.
#[test]
fn only_a_first_hello_is_taken_back() {
    let told = Arc::new(Gone::default());
    let fx = with_placer(config(3), told.clone());
    assert!(!fx.registry.withdraw_first_hello(&host("a"), 1));
    fx.join("a", "t");
    fx.pass_grace();
    assert_eq!(fx.join("a", "t"), 2);
    assert!(!fx.registry.withdraw_first_hello(&host("a"), 2));
    assert!(!fx.registry.withdraw_first_hello(&host("a"), 1));
    assert!(fx.registry.own_host(&host("a")).is_some());
    assert!(told.0.lock().unwrap().is_empty());
}
