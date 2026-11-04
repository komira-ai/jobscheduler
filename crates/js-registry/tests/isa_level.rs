//! The architecture and the ISA level through the registry: what a hello must report to
//! be admitted to a pool that names them, where a request that asks for them is placed, and
//! what a waiting request tells a scaler.

mod support;

use js_agent_wire::Hello;
use js_provider::{Provider, RefusalCode, StartAck, StartSpec};
use js_registry::{Launchable, PoolConfig, Refusal, WholeNodeConfig};
use js_types::{ArmVersion, IsaLevel, ProviderId, Request, Shape};
use support::world::*;
use support::*;

/// A pool whose shape names `pairs` besides `os`.
fn pool_naming(pairs: &[(&str, &str)]) -> Fixture {
    let mut capabilities = map(&[("os", "macos")]);
    capabilities.extend(map(pairs));
    let shape = Shape::new(pool_shape().resources().clone(), capabilities);
    let name = ProviderId::new("builders").unwrap();
    fixture_of(PoolConfig::new(name, class(), shape, 4))
}

/// A hello that reports `pairs` besides `os`.
fn reporting(host: &str, pairs: &[(&str, &str)]) -> Hello {
    let mut hello = hello(host);
    hello.capabilities.extend(map(pairs));
    hello
}

/// The start of lease 1 for a request that requires `pairs` besides `os`.
fn wanting(pairs: &[(&str, &str)]) -> StartSpec {
    let mut asked: Request = request();
    asked.capabilities.extend(map(pairs));
    StartSpec::new(lease(1), asked, &[class()]).unwrap()
}

/// Asserts that a pool whose shape names the list `x86-64-v1,x86-64-v2` admits a host that
/// reports that same list (the configuration and hello a release that compared the texts
/// accepted) and one that reports a higher level, and refuses `shape-not-accepted` a host
/// below the list, one of the other family and one that reports no level. Catches a
/// floor written as a list that no hello can meet any more.
#[test]
fn a_floor_that_lists_levels_admits_the_hosts_it_admitted_before() {
    let list = "x86-64-v1,x86-64-v2";
    let fx = pool_naming(&[("isa_level", list)]);
    fx.hello(&reporting("same", &[("isa_level", list)]), &cred("t"))
        .unwrap();
    fx.hello(&reporting("v3", &[("isa_level", "x86-64-v3")]), &cred("t"))
        .unwrap();
    for (host, pairs) in [
        ("v1", &[("isa_level", "x86-64-v1")][..]),
        ("arm", &[("isa_level", "armv8.4-a")][..]),
        ("bare", &[][..]),
    ] {
        let refused = fx.hello(&reporting(host, pairs), &cred("t"));
        assert_eq!(refused.unwrap_err(), Refusal::ShapeNotAccepted, "{host}");
    }
    assert_eq!(fx.registry.hosts().len(), 2);
}

/// The answer to a start of `pairs` on a pool that books `x86-64-v3`, whose one host
/// reports `x86-64-v4`.
fn placed_on_a_v3_pool(pairs: &[(&str, &str)]) -> StartAck {
    let mut fx = pool_naming(&[("isa_level", "x86-64-v3")]);
    fx.hello(&reporting("h", &[("isa_level", "x86-64-v4")]), &cred("t"))
        .unwrap();
    fx.provider.start(wanting(pairs))
}

/// Asserts that a request for a level is placed by the level the pool books, which every
/// host was held to: on a pool that books v3, a request for v3, v2 or no level is placed;
/// one for v4 is refused though the host reports v4 (the pool does not book it, and the
/// scaler's offer of the host states v3 too); one of the other family is refused. Also
/// that a pool which books no level places no level request. Catches a level compared as
/// text (v2 refused on a v3 host) or across families.
#[test]
fn a_request_for_a_level_is_placed_by_the_level_the_pool_books() {
    for level in ["x86-64-v3", "x86-64-v2"] {
        let ack = placed_on_a_v3_pool(&[("isa_level", level)]);
        assert_eq!(started_on(&ack), Some("h"), "{level}");
    }
    assert_eq!(started_on(&placed_on_a_v3_pool(&[])), Some("h"));
    let no_fit = StartAck::Refused(RefusalCode::NoFit);
    for level in ["x86-64-v4", "armv8.2-a"] {
        assert_eq!(
            placed_on_a_v3_pool(&[("isa_level", level)]),
            no_fit,
            "{level}"
        );
    }
    let mut bare_pool = fixture(4);
    bare_pool.join("a", "t");
    let asking = bare_pool
        .provider
        .start(wanting(&[("isa_level", "x86-64-v1")]));
    assert_eq!(asking, no_fit);
}

/// The answer to a start of `pairs` on a pool whose shape says `arch = "aarch64"`, whose
/// one host is a new agent reporting `arm64`.
fn placed_on_an_aarch64_pool(pairs: &[(&str, &str)]) -> StartAck {
    let mut fx = pool_naming(&[("arch", "aarch64")]);
    fx.hello(&reporting("h", &[("arch", "arm64")]), &cred("t"))
        .unwrap();
    fx.provider.start(wanting(pairs))
}

/// Asserts that a pool whose configuration says `arch = "aarch64"` admits a new agent that
/// reports `arm64`, places on it a request for `arch=arm64` and one for `arch=aarch64`, and
/// refuses one for `x86_64`. Catches the text comparison that would turn every new agent
/// away from a pool configured before `arm64` was the one spelling.
#[test]
fn a_pool_configured_aarch64_admits_and_serves_an_arm64_agent() {
    for arch in ["arm64", "aarch64"] {
        let ack = placed_on_an_aarch64_pool(&[("arch", arch)]);
        assert_eq!(started_on(&ack), Some("h"), "{arch}");
    }
    assert_eq!(
        placed_on_an_aarch64_pool(&[("arch", "x86_64")]),
        StartAck::Refused(RefusalCode::NoFit)
    );
}

/// Asserts that a whole-node request for a level, in a pool that books an Arm level at
/// least as high but lists no host yet and could launch one, waits, and that the wait list tells the scaler the level it asked
/// for (and the canonical architecture), as the launchable was told. Catches a demand that
/// drops the level between the request and the scaler, which would launch a machine of
/// any level for it.
#[test]
fn a_waiting_request_keeps_its_level() {
    let shape = Shape::new(
        pool_shape().resources().clone(),
        map(&[
            ("os", "macos"),
            ("arch", "arm64"),
            ("isa_level", "armv8.4-a"),
        ]),
    );
    let name = ProviderId::new("builders").unwrap();
    let config = PoolConfig::new(name, whole(), shape, 4)
        .with_whole_node(WholeNodeConfig::dedicated(4))
        .unwrap();
    let mut w = World::of(config, &[]);
    w.fx.registry
        .set_launchable(Some(Launchable::new(|demand| demand.isa_level.is_some())));
    let mut asked = Request {
        class: whole().id().clone(),
        ..request()
    };
    asked
        .capabilities
        .extend(map(&[("isa_level", "armv8.2-a"), ("arch", "aarch64")]));
    let spec = StartSpec::new(lease(1), asked, &[whole()])
        .unwrap()
        .with_request_id(id("r1"));
    assert_eq!(w.provider.start(spec), StartAck::Unknown);
    let waiting = w.fx.registry.waiting();
    assert_eq!(waiting.len(), 1);
    assert_eq!(
        waiting[0].demand.isa_level,
        Some(IsaLevel::Arm64(ArmVersion::V8_2))
    );
    assert_eq!(waiting[0].demand.arch.as_deref(), Some("arm64"));
}
