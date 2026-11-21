//! What a server needs of the registry to place over the hosts' `node/` rows in the job store:
//! the times that judge a row live and forget it, and a pool's machine cap and its booking rule.

mod support;

use std::time::Duration;

use js_agent_wire::DEFAULT_GRACE_MS;
use js_registry::{DEFAULT_LOST_RETENTION_MS, PoolConfig, Registry, SequentialSessions};
use js_types::{ProviderId, Resources, Shape};
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

/// Asserts the times of a registry over one pool: the grace it serves, and the grace plus the
/// default lost-host retention to forget a row. It catches a row
/// judged live by another grace than the registry's, and a forget sooner than the registry
/// forgets the host.
#[test]
fn one_pools_times_are_its_grace_and_the_grace_plus_retention() {
    let fx = fixture(3);
    let grace = Duration::from_millis(DEFAULT_GRACE_MS);
    assert_eq!(fx.registry.longest_grace(), grace);
    let retention = Duration::from_millis(DEFAULT_LOST_RETENTION_MS);
    assert_eq!(fx.registry.forget_after(), grace + retention);
}

/// Asserts the times of a registry over two pools: a row is live for the longest grace, and is
/// forgotten after the longest grace plus the longest retention. It catches a row of the pool with the longer grace judged lost by the shorter
/// one's, and a row forgotten while the longer retention still keeps its host.
#[test]
fn several_pools_times_are_the_longest_grace_and_the_longest_retention() {
    let fx = two_pools();
    let longest = Duration::from_millis(DEFAULT_GRACE_MS);
    assert_eq!(fx.registry.longest_grace(), longest);
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
