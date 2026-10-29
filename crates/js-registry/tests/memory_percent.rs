//! The share of a pool's memory that is booked: `PoolConfig::with_memory_percent`.

mod support;

use js_agent_wire::{Isolation, JobSpec, Limits, refusal};
use js_provider::{Provider, RefusalCode, StartAck, StartSpec};
use js_registry::{PoolConfig, Refusal, RunRefusal, WholeNodeConfig};
use js_types::{ExecMode, MemoryPercent, ProviderId, Request, RequestId, ResourceClass, Resources};
use support::world::{HOUR_S, World};
use support::*;

/// The memory the pool lists: 8 GiB.
const LISTED: u64 = 8 << 30;

/// What 90 percent of it books, rounded down: 7730941132.8 bytes.
const NINETY: u64 = LISTED * 90 / 100;

fn percent(value: u64) -> MemoryPercent {
    MemoryPercent::new(value).unwrap()
}

fn ninety() -> PoolConfig {
    config(4).with_memory_percent(percent(90))
}

fn asking(sequence: u64, memory_bytes: u64) -> StartSpec {
    let wanted = Request {
        resources: Resources::new(1000, memory_bytes, 0, None).unwrap(),
        ..request()
    };
    StartSpec::new(lease(sequence), wanted, &[class()]).unwrap()
}

fn job(memory_bytes: u64) -> JobSpec {
    JobSpec {
        run_id: "r1".into(),
        lease_id: String::new(),
        image: "busybox:1@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
            .into(),
        args: Vec::new(),
        limits: Limits {
            cpu_millis: 1000,
            memory_bytes,
            pids: None,
            wall_ms: 60_000,
        },
        gpu_indices: Vec::new(),
        mounts: Vec::new(),
        isolation: Isolation::Contained,
        labels: Default::default(),
        heartbeat_seq: 0,
        valid_for_ms: 0,
        native: None,
    }
}

/// Asserts that at 90 a pool of 8 GiB hosts books floor(8 GiB x 90 / 100) bytes: a lease
/// of that many is placed and one byte more finds no fit, although the host's own claim
/// (16 GiB) is larger than both. It catches a percent that is not applied, one rounded
/// up, and a booking that follows the claim.
#[test]
fn ninety_percent_books_exactly_the_cut_memory() {
    assert_eq!(NINETY, 7_730_941_132);
    let mut fx = fixture_of(ninety());
    fx.join("a", "t");
    assert_eq!(started_on(&fx.provider.start(asking(1, NINETY))), Some("a"));
    let mut fx = fixture_of(ninety());
    fx.join("a", "t");
    let over = fx.provider.start(asking(1, NINETY + 1));
    assert_eq!(over, StartAck::Refused(RefusalCode::NoFit));
}

/// Asserts that a pool without a percent books all of the listed memory and one byte
/// more is refused: the default of 100 changes nothing.
#[test]
fn the_default_books_all_of_the_listed_memory() {
    let mut fx = fixture(4);
    fx.join("a", "t");
    assert_eq!(started_on(&fx.provider.start(asking(1, LISTED))), Some("a"));
    let mut fx = fixture(4);
    fx.join("a", "t");
    let over = fx.provider.start(asking(1, LISTED + 1));
    assert_eq!(over, StartAck::Refused(RefusalCode::NoFit));
    assert_eq!(config(4).memory_percent(), MemoryPercent::FULL);
}

/// Asserts that a host claiming far more than the pool lists is booked at the cut
/// memory all the same, in the listing the provider gives the lease table and in the
/// registry's own view. It catches a booking that follows the agent's claim, which would
/// let an agent inflate a lease.
#[test]
fn a_larger_claim_never_raises_what_is_booked() {
    let fx = fixture_of(ninety());
    let mut big = hello("big");
    big.shape.memory_bytes = 512 << 30;
    fx.hello(&big, &cred("t")).unwrap();
    let listed = fx.provider.capacity();
    assert_eq!(listed[0].shape.resources().memory_bytes(), NINETY);
    assert_eq!(listed[0].memory.bookable_bytes(), NINETY);
    let view = &fx.registry.hosts()[0];
    assert_eq!(view.shape.resources().memory_bytes(), NINETY);
    let mut fx = fixture_of(ninety());
    fx.hello(&big, &cred("t")).unwrap();
    let over = fx.provider.start(asking(1, NINETY + 1));
    assert_eq!(over, StartAck::Refused(RefusalCode::NoFit));
}

/// Asserts that the hello is held to the whole listed memory, not the cut: a host that
/// claims the cut memory (one byte under the listing) is refused `shape-not-accepted`
/// and a host that claims exactly the listing joins. The host must really have the
/// memory; the pool books less of it.
#[test]
fn a_hello_is_held_to_the_unreduced_memory() {
    let fx = fixture_of(ninety());
    let mut short = hello("short");
    short.shape.memory_bytes = NINETY;
    let refused = fx.registry.hello(&short, &cred("t")).unwrap_err();
    assert_eq!(refused, Refusal::ShapeNotAccepted);
    assert_eq!(refused.code(), refusal::SHAPE_NOT_ACCEPTED);
    let mut under = hello("under");
    under.shape.memory_bytes = LISTED - 1;
    assert_eq!(
        fx.registry.hello(&under, &cred("t")).unwrap_err(),
        Refusal::ShapeNotAccepted
    );
    let mut exact = hello("exact");
    exact.shape.memory_bytes = LISTED;
    assert!(fx.hello(&exact, &cred("t")).is_ok());
    assert_eq!(fx.registry.hosts().len(), 1);
}

/// Asserts that a run's memory limit is bound by the cut memory: the exact cut is queued
/// and one byte more is refused `limits-exceed-shape`. It catches a run bound that still
/// reads the unreduced shape, which would let a job use the memory the pool keeps back.
#[test]
fn a_run_is_bound_by_the_cut_memory() {
    let mut fx = fixture_of(ninety());
    fx.join("a", "t");
    assert_eq!(started_on(&fx.provider.start(asking(1, 1))), Some("a"));
    let queued = fx.registry.queue_run(&host("a"), &lease(1), job(NINETY));
    assert!(queued.is_ok(), "{queued:?}");
    let mut over = job(NINETY + 1);
    over.run_id = "r2".into();
    let refused = fx.registry.queue_run(&host("a"), &lease(1), over);
    assert_eq!(refused.unwrap_err(), RunRefusal::LimitsExceedShape);
}

/// Asserts that a whole-node pool books the cut memory too: a request of the exact cut is
/// committed (the whole host is held, not ready yet) and one byte more finds no fit.
#[test]
fn a_whole_node_request_sees_the_cut_memory() {
    let whole = || {
        let name = ProviderId::new("builders").unwrap();
        let pool = PoolConfig::new(name, ResourceClass::whole_node(), pool_shape(), 4);
        let pool = pool.with_whole_node(WholeNodeConfig::dedicated(4)).unwrap();
        pool.with_memory_percent(percent(90))
    };
    let ask = |sequence: u64, memory_bytes: u64| {
        let wanted = Request {
            class: ResourceClass::whole_node().id().clone(),
            resources: Resources::new(1000, memory_bytes, 0, None).unwrap(),
            ..request()
        };
        StartSpec::new(lease(sequence), wanted, &[ResourceClass::whole_node()]).unwrap()
    };
    let mut fx = fixture_of(whole());
    fx.join("a", "t");
    assert_eq!(fx.provider.start(ask(1, NINETY)), StartAck::Unknown);
    assert_eq!(fx.provider.booked().len(), 1);
    let mut fx = fixture_of(whole());
    fx.join("a", "t");
    let over = fx.provider.start(ask(1, NINETY + 1));
    assert_eq!(over, StartAck::Refused(RefusalCode::NoFit));
}

/// Asserts that a whole-node booking that has to wait for a busy host sees the cut memory
/// too: a request of the exact cut reserves the host (a draining mark) and commits it when
/// the host's lease ends, and a request one byte over the cut is refused `no-fit` and
/// leaves no mark. It catches a reservation path that reads the unreduced shape.
#[test]
fn a_reserved_host_is_held_to_the_cut_memory() {
    let whole_ninety = || {
        let name = ProviderId::new("builders").unwrap();
        let pool = PoolConfig::new(name, ResourceClass::whole_node(), pool_shape(), 4);
        let pool = pool.with_whole_node(WholeNodeConfig::dedicated(4)).unwrap();
        pool.with_memory_percent(percent(90))
    };
    let ask = |sequence: u64, name: &str, memory_bytes: u64| {
        let wanted = Request {
            class: ResourceClass::whole_node().id().clone(),
            resources: Resources::new(1000, memory_bytes, 0, None).unwrap(),
            ..request()
        };
        let spec = StartSpec::new(lease(sequence), wanted, &[ResourceClass::whole_node()]);
        spec.unwrap().with_request_id(RequestId::new(name).unwrap())
    };
    let mut exact = World::of(whole_ninety(), &["a"]);
    exact.share(1, HOUR_S);
    exact.provider.start(ask(2, "r1", NINETY));
    exact.check();
    assert_eq!(exact.reserved_host("r1"), Some("a".to_string()));
    exact.stop(1);
    exact.fx.registry.advance();
    exact.provider.start(ask(2, "r1", NINETY));
    exact.check();
    assert_eq!(exact.lease_on("a"), Some(lease(2)));
    let mut over = World::of(whole_ninety(), &["a"]);
    over.share(1, HOUR_S);
    let refused = over.provider.start(ask(2, "r1", NINETY + 1));
    over.check();
    assert_eq!(refused, StartAck::Refused(RefusalCode::NoFit));
    assert_eq!(over.reserved_host("r1"), None);
}

/// Asserts that the registry's view lists the memory the pool lists, the percent and the
/// bookable memory, and that a pool left at the default lists the percent as 100. It
/// catches a view that shows the unreduced figure as the bookable one.
#[test]
fn the_view_shows_listed_percent_and_bookable_memory() {
    let fx = fixture_of(ninety());
    fx.join("a", "t");
    let view = &fx.registry.hosts()[0];
    assert_eq!(view.memory.configured_bytes(), LISTED);
    assert_eq!(view.memory.percent().get(), 90);
    assert_eq!(view.memory.bookable_bytes(), NINETY);
    let plain = fixture(4);
    plain.join("a", "t");
    let view = &plain.registry.hosts()[0];
    assert_eq!(view.memory.percent(), MemoryPercent::FULL);
    assert_eq!(view.memory.bookable_bytes(), LISTED);
}

/// Asserts that the booked shape is the same whichever of the percent and the
/// execution mode is set first: the cut and the mode are both kept. It catches a setter
/// that rebuilds the booked shape from the unreduced one and drops the other.
#[test]
fn the_percent_and_the_mode_may_be_set_in_either_order() {
    let a = config(4)
        .with_memory_percent(percent(90))
        .with_exec(ExecMode::Container);
    let b = config(4)
        .with_exec(ExecMode::Container)
        .with_memory_percent(percent(90));
    assert_eq!(a, b);
    let fx = fixture_of(a);
    let mut claim = hello("a");
    claim.capabilities.extend(map(&[("runtime", "container")]));
    fx.hello(&claim, &cred("t")).unwrap();
    let booked = fx.registry.hosts()[0].shape.clone();
    assert_eq!(booked.resources().memory_bytes(), NINETY);
    assert_eq!(
        booked.capabilities().get("exec").map(String::as_str),
        Some("container")
    );
}
