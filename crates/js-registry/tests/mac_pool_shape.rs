//! Admission of Mac Studio hosts by a pool's one shape: a host's capacity is what it reports,
//! and the pool's `[shape]` is the most the pool books of any host. A host offering more is
//! booked at the shape; a host offering less joins and is booked at what it offers. Only the
//! capabilities are held to the pool's: a host reporting another `arch` or `os` is refused
//! `shape-not-accepted` (`below-floor`). A Linux guest on the Macs, which reports its own
//! cores and memory and another `os`, joins a pool of its own with a shape that fits the
//! guest.

mod support;

use js_agent_wire::{Hello, refusal};
use js_registry::{PoolConfig, Refusal, Registry, SequentialSessions, ShapeFault};
use js_types::{ProviderId, Resources, Shape};
use support::*;

/// The cores of the smallest Mac, in millicores.
const SMALL_MAC_CPU: u64 = 28_000;

/// The cores of the largest Mac, in millicores.
const LARGE_MAC_CPU: u64 = 32_000;

/// The memory of each Mac: 96 GiB.
const MAC_MEMORY: u64 = 96 << 30;

/// The cores of the native pool beside a guest pool on the same Macs: 10 cores. With
/// [`GUEST_CPU`] it comes to 24 cores, within the smallest Mac.
const NATIVE_CPU: u64 = 10_000;

/// The memory of the native pool beside a guest pool: 36 GiB. With [`GUEST_MEMORY`] it comes
/// to 76 GiB, within a Mac's 96.
const NATIVE_MEMORY: u64 = 36 << 30;

/// The cores of a Linux guest on each Mac and of its pool: 14 cores.
const GUEST_CPU: u64 = 14_000;

/// The memory of a Linux guest on each Mac and of its pool: 40 GiB.
const GUEST_MEMORY: u64 = 40 << 30;

/// A macOS pool on [`ARCH`] called `builders` whose one shape is `cpu_millis` and
/// `memory_bytes`.
fn mac_pool(cpu_millis: u64, memory_bytes: u64) -> PoolConfig {
    let resources = Resources::new(cpu_millis, memory_bytes, 0, None).unwrap();
    let shape = Shape::new(resources, map(&[("os", "macos"), ("arch", ARCH)]));
    PoolConfig::new(ProviderId::new("builders").unwrap(), class(), shape, 4)
}

/// The pool shaped at the smallest Mac: 28 cores, 96 GiB.
fn small_mac_pool() -> PoolConfig {
    mac_pool(SMALL_MAC_CPU, MAC_MEMORY)
}

/// A hello for `host` in pool `builders` claiming `cpu_millis` and `memory_bytes`.
fn mac_hello(host: &str, cpu_millis: u64, memory_bytes: u64) -> Hello {
    let mut claim = hello(host);
    claim.shape.cpu_millis = cpu_millis;
    claim.shape.memory_bytes = memory_bytes;
    claim
}

/// Asserts that `hello` is refused `shape-not-accepted` with the reason `below-floor`.
fn assert_below_floor(fx: &Fixture, hello: &Hello) {
    let refused = fx.registry.hello(hello, &cred("t")).unwrap_err();
    assert_eq!(refused, Refusal::ShapeNotAccepted(ShapeFault::BelowFloor));
    assert_eq!(refused.code(), refusal::SHAPE_NOT_ACCEPTED);
    assert_eq!(refused.reason(), Some(refusal::SHAPE_BELOW_FLOOR));
}

/// Asserts that a Mac offering exactly the pool's shape (28000 millicores, 103079215104
/// bytes) joins and is booked at that shape, the cap and the report being equal.
#[test]
fn a_mac_offering_exactly_the_pool_shape_joins() {
    assert_eq!(MAC_MEMORY, 103_079_215_104);
    let fx = fixture_of(small_mac_pool());
    fx.hello(&mac_hello("mac-28", SMALL_MAC_CPU, MAC_MEMORY), &cred("t"))
        .unwrap();
    let view = &fx.registry.hosts()[0];
    assert_eq!(view.id.as_str(), "mac-28");
    assert_eq!(view.shape.resources().cpu_millis(), SMALL_MAC_CPU);
    assert_eq!(view.shape.resources().memory_bytes(), MAC_MEMORY);
}

/// Asserts that a 32-core Mac joins the pool shaped at 28 cores and is booked at 28: the
/// pool's one shape is what it books, and the 4 further cores are kept only in the host's
/// claim. A pool cannot book its Macs at different sizes.
#[test]
fn a_32_core_mac_joins_and_is_booked_at_the_pool_shape() {
    let fx = fixture_of(small_mac_pool());
    fx.hello(&mac_hello("mac-32", LARGE_MAC_CPU, MAC_MEMORY), &cred("t"))
        .unwrap();
    let view = &fx.registry.hosts()[0];
    assert_eq!(view.shape.resources().cpu_millis(), SMALL_MAC_CPU);
    assert_eq!(view.shape.resources().memory_bytes(), MAC_MEMORY);
    let claim = view.claim.as_ref().unwrap();
    assert_eq!(claim.resources().cpu_millis(), LARGE_MAC_CPU);
}

/// The CPU and memory `fx` lists host `name` at.
fn listed(fx: &Fixture, name: &str) -> (u64, u64) {
    let hosts = fx.registry.hosts();
    let view = hosts.iter().find(|host| host.id.as_str() == name).unwrap();
    let resources = view.shape.resources();
    (resources.cpu_millis(), resources.memory_bytes())
}

/// Asserts that a Mac offering less than the pool's shape joins and is booked at what it
/// offers: one millicore under, one byte under, both under, and a whole host of fewer cores
/// (24) are each listed at their own cores and memory. A host of more than the shape on
/// `x86_64` where the shape names `arm64` is still refused `below-floor` and leaves no
/// record: the architecture is a capability, not an amount. It catches a floor that still
/// refuses a smaller host, and a booking at the pool's numbers that would book cores or
/// memory the host does not have.
#[test]
fn a_mac_offering_less_than_the_pool_shape_joins_at_what_it_offers() {
    let fx = fixture_of(small_mac_pool());
    let short = [
        ("cpu-under", SMALL_MAC_CPU - 1, MAC_MEMORY),
        ("memory-under", SMALL_MAC_CPU, MAC_MEMORY - 1),
        ("both-under", SMALL_MAC_CPU - 1, MAC_MEMORY - 1),
        ("mac-24", 24_000, MAC_MEMORY),
    ];
    for (name, cpu, memory) in short {
        fx.hello(&mac_hello(name, cpu, memory), &cred("t")).unwrap();
        assert_eq!(listed(&fx, name), (cpu, memory), "{name}");
    }
    let mut other_arch = mac_hello("other-arch", LARGE_MAC_CPU, MAC_MEMORY);
    other_arch.capabilities = map(&[("os", "macos"), ("arch", "x86_64")]);
    assert_below_floor(&fx, &other_arch);
    let hosts = fx.registry.hosts();
    assert_eq!(hosts.len(), 4);
    assert!(hosts.iter().all(|host| host.id.as_str() != "other-arch"));
}

/// Asserts that a pool shaped at the largest Mac (32 cores) admits both the 28-core Mac and
/// the 32-core one, each booked at its own cores: a pool shape sized to the larger Macs no
/// longer leaves the smaller ones out.
#[test]
fn a_pool_shaped_at_32_cores_admits_the_28_core_mac_at_28() {
    let fx = fixture_of(mac_pool(LARGE_MAC_CPU, MAC_MEMORY));
    fx.hello(&mac_hello("mac-28", SMALL_MAC_CPU, MAC_MEMORY), &cred("t"))
        .unwrap();
    fx.hello(&mac_hello("mac-32", LARGE_MAC_CPU, MAC_MEMORY), &cred("t"))
        .unwrap();
    assert_eq!(listed(&fx, "mac-28"), (SMALL_MAC_CPU, MAC_MEMORY));
    assert_eq!(listed(&fx, "mac-32"), (LARGE_MAC_CPU, MAC_MEMORY));
}

/// The pool `guests`: Linux on [`ARCH`] at [`GUEST_CPU`] and [`GUEST_MEMORY`].
fn guest_pool() -> PoolConfig {
    let resources = Resources::new(GUEST_CPU, GUEST_MEMORY, 0, None).unwrap();
    let shape = Shape::new(resources, map(&[("os", "linux"), ("arch", ARCH)]));
    PoolConfig::new(ProviderId::new("guests").unwrap(), class(), shape, 4)
}

/// A hello from the Linux guest of a Mac: pool `guests`, the guest's own cores and memory.
fn guest_hello(host: &str) -> Hello {
    let mut claim = mac_hello(host, GUEST_CPU, GUEST_MEMORY);
    claim.pool = "guests".into();
    claim.capabilities = map(&[("os", "linux"), ("arch", ARCH)]);
    claim
}

/// Asserts that one 28-core Mac serves two pools when each has its own shape that fits it:
/// its native agent joins the native pool (10 cores, 36 GiB) and its Linux guest joins the
/// guest pool (14 cores, 40 GiB), each booked at its own pool's shape. The guest naming the native pool is refused
/// `below-floor` although it offers more cores and memory than that pool's shape, because
/// it reports `os` `linux` and the shape names `macos`: the two kinds of host cannot share
/// one pool and one shape.
#[test]
fn native_and_guest_pools_each_shaped_to_fit_a_28_core_mac_admit_both() {
    let native_pool = mac_pool(NATIVE_CPU, NATIVE_MEMORY);
    let mut fx = fixture_of(native_pool.clone());
    let pools = (
        native_pool.booking_whole_memory(),
        vec![guest_pool().booking_whole_memory()],
    );
    let placer = fx.placer.clone();
    let sessions = SequentialSessions::new();
    fx.registry = Registry::with_pools(pools, fx.clock.clone(), sessions, placer).unwrap();

    let mut misplaced = guest_hello("mac-28-guest");
    misplaced.pool = "builders".into();
    assert_below_floor(&fx, &misplaced);

    let native = fx
        .hello(&mac_hello("mac-28", SMALL_MAC_CPU, MAC_MEMORY), &cred("t"))
        .unwrap();
    let guest = fx.hello(&guest_hello("mac-28-guest"), &cred("t")).unwrap();
    assert_eq!(native.pool, "builders");
    assert_eq!(guest.pool, "guests");

    let hosts = fx.registry.hosts();
    let booked = |id: &str| {
        let view = hosts.iter().find(|view| view.id.as_str() == id).unwrap();
        let resources = view.shape.resources();
        (resources.cpu_millis(), resources.memory_bytes())
    };
    assert_eq!(booked("mac-28"), (NATIVE_CPU, NATIVE_MEMORY));
    assert_eq!(booked("mac-28-guest"), (GUEST_CPU, GUEST_MEMORY));
}
