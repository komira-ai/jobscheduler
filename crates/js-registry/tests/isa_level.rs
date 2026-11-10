//! The architecture and the ISA level through the registry: what a hello must report to
//! be admitted to a pool that names them, and where a request that asks for the architecture is
//! placed.

mod support;

use js_agent_wire::Hello;
use js_registry::{PoolConfig, Refusal, ShapeFault};
use js_types::{ProviderId, Request, Shape};
use support::*;

/// A pool whose shape names `pairs` besides `os` and the architecture of [`ARCH`] (a shape
/// without one has no scheduler form, so none of its hosts is a worker), which `pairs` may
/// replace.
fn pool_naming(pairs: &[(&str, &str)]) -> Fixture {
    let mut capabilities = map(&[("os", "macos"), ("arch", ARCH)]);
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

/// A request that requires `pairs` besides `os`.
fn wanting(pairs: &[(&str, &str)]) -> Request {
    let mut asked: Request = request();
    asked.capabilities.extend(map(pairs));
    asked
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
        assert_eq!(
            refused.unwrap_err(),
            Refusal::ShapeNotAccepted(ShapeFault::BelowFloor),
            "{host}"
        );
    }
    assert_eq!(fx.registry.hosts().len(), 2);
}

/// Whether a request for `pairs` is placed on the one host of a pool whose shape says
/// `arch = "aarch64"`, a new agent reporting `arm64`.
fn placed_on_an_aarch64_pool(pairs: &[(&str, &str)]) -> bool {
    let fx = pool_naming(&[("arch", "aarch64")]);
    fx.hello(&reporting("h", &[("arch", "arm64")]), &cred("t"))
        .unwrap();
    placed_on(&fx, "h", &wanting(pairs))
}

/// Asserts that a pool whose configuration says `arch = "aarch64"` admits a new agent that
/// reports `arm64`, places on it a request for `arch=arm64` and one for `arch=aarch64`, and
/// not one for `x86_64`. Catches the text comparison that would turn every new agent away
/// from a pool configured before `arm64` was the one spelling.
#[test]
fn a_pool_configured_aarch64_admits_and_serves_an_arm64_agent() {
    for arch in ["arm64", "aarch64"] {
        assert!(placed_on_an_aarch64_pool(&[("arch", arch)]), "{arch}");
    }
    assert!(!placed_on_an_aarch64_pool(&[("arch", "x86_64")]));
}
