//! A pool that places each host by what its hello reported (`PoolConfig::placing_by_hello`,
//! the lease server's elastic pools): its shape is a floor that names no `arch`, every host
//! is booked as the floor and placed by the capabilities its agent reported on top of it,
//! and a request fits the pool by the capabilities the floor names. A pool without the
//! setting (an agent or joined pool) places every host on its own shape, as before.

mod support;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use js_provider::{Candidate, Pool, RefusalCode, StartAck, StartSpec};
use js_registry::{MarkedTails, Placer, PoolConfig, RegisteredProvider, Registry};
use js_types::{HostId, ProviderId, Request, Resources, Shape};
use js_types::{ManualClock, Timestamp};
use support::*;

/// The floor of a pool whose hosts are launched from offers: 4 cores and 8 GiB, no
/// capability at all, as the lease server's elastic pool books it.
fn floor() -> Shape {
    Shape::new(
        Resources::new(4000, 8 << 30, 0, None).unwrap(),
        BTreeMap::new(),
    )
}

/// A pool of up to four hosts of [`floor`] that places each host by its hello.
fn by_hello() -> PoolConfig {
    let name = ProviderId::new("builders").unwrap();
    PoolConfig::new(name, class(), floor(), 4).placing_by_hello()
}

/// [`hello`] of `host` reporting `arch` (and the `os` every [`hello`] reports).
fn hello_of(host: &str, arch: &str) -> js_agent_wire::Hello {
    let mut said = hello(host);
    said.capabilities.insert("arch".into(), arch.into());
    said
}

/// A lease numbered `sequence` of the pool's class that requires `capabilities`.
fn needing(sequence: u64, capabilities: &[(&str, &str)]) -> StartSpec {
    let request = Request {
        capabilities: map(capabilities),
        ..request()
    };
    StartSpec::new(lease(sequence), request, &[class()]).unwrap()
}

/// A placer that books the first candidate and writes down each host it is told is up,
/// with the capabilities of the shape it is told.
#[derive(Default)]
struct Shapes {
    said: Mutex<Vec<String>>,
}

impl Shapes {
    fn said(&self) -> Vec<String> {
        std::mem::take(&mut *self.said.lock().unwrap())
    }
}

impl Placer for Shapes {
    fn pick(&self, _: &StartSpec, _: &[Candidate], _: &MarkedTails) -> Option<usize> {
        Some(0)
    }

    fn host_up(&self, host: &HostId, shape: &Shape) {
        let caps: Vec<String> = shape
            .capabilities()
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect();
        let cpu = shape.resources().cpu_millis();
        let line = format!("up {} {cpu} {}", host.as_str(), caps.join(","));
        self.said.lock().unwrap().push(line);
    }
}

/// A registry over `config` whose placer is `told` ([`Shapes`]), as the server sets a
/// registry up (each host's whole memory booked), and its provider and clock.
fn told_by(config: PoolConfig, told: &Arc<Shapes>) -> (Registry, RegisteredProvider, ManualClock) {
    let clock = ManualClock::new(Timestamp::from_millis(1_000_000));
    let registry = Registry::new(config.booking_whole_memory(), clock.clone(), told.clone());
    let provider = registry.provider();
    (registry, provider, clock)
}

/// Says hello for `said` to `registry` with the credential every test uses.
fn say(registry: &Registry, said: &js_agent_wire::Hello) {
    registry.hello(said, &cred("t")).unwrap();
}

/// Catches: a placer told the pool's booked shape in place of the host's (an elastic host
/// that is no node of js-sched, having no `arch`), told the host's claimed numbers in place
/// of the pool's, or told the reported capabilities in a pool that does not place by
/// hello. The host claims 8 cores and reports `arch`, `os` and `runtime`: in a pool that
/// places by hello the placer hears the pool's 4 cores with all three; in a pool of the
/// same floor without the setting it hears the floor alone, as an agent or joined pool
/// always told it.
#[test]
fn the_placer_is_told_the_capabilities_the_hello_reported_only_when_the_pool_places_by_them() {
    let mut said = hello_of("a", "arm64");
    said.capabilities
        .insert("runtime".into(), "container".into());
    let placed = |config: PoolConfig| {
        let told = Arc::new(Shapes::default());
        let (registry, _, _) = told_by(config, &told);
        say(&registry, &said);
        told.said()
    };
    assert_eq!(
        placed(by_hello()),
        ["up a 4000 arch=arm64,os=macos,runtime=container"]
    );
    let name = ProviderId::new("builders").unwrap();
    let plain = PoolConfig::new(name, class(), floor(), 4);
    assert_eq!(placed(plain), ["up a 4000 "]);
}

/// Catches: the reported value written over the pool's where both name a key (a pool that
/// promises `isa_level=x86-64-v2` would then book a host by a level it does not promise),
/// or a reported key that the pool names dropped. The pool names `os` and `isa_level`; the
/// host reports a higher level, its `arch` and the same `os`: the placer and the book see
/// the pool's level and `os`, with the host's `arch`.
#[test]
fn the_pools_value_stays_where_the_pool_and_the_hello_both_name_a_key() {
    let name = ProviderId::new("builders").unwrap();
    let shape = Shape::new(
        floor().resources().clone(),
        map(&[("os", "macos"), ("isa_level", "x86-64-v2")]),
    );
    let config = PoolConfig::new(name, class(), shape, 4).placing_by_hello();
    let told = Arc::new(Shapes::default());
    let (registry, provider, _) = told_by(config, &told);
    let mut said = hello_of("a", "x86_64");
    said.capabilities
        .insert("isa_level".into(), "x86-64-v3".into());
    say(&registry, &said);
    assert_eq!(
        told.said(),
        ["up a 4000 arch=x86_64,isa_level=x86-64-v2,os=macos"]
    );
    let listed = provider.capacity();
    assert_eq!(
        listed[0].shape.capabilities(),
        &map(&[
            ("arch", "x86_64"),
            ("isa_level", "x86-64-v2"),
            ("os", "macos")
        ])
    );
}

/// Catches: a host that holds a lease kept on the shape it was booked with in a pool that
/// places by hello, and a placer told another shape than the book lists (the engine and the
/// book would disagree at every start, and the engine be rebuilt at each). The host is
/// booked as `arm64`, goes silent and says hello again reporting `x86_64` while it holds
/// the lease: the book lists it as `x86_64` with its lease kept and the placer is told
/// `x86_64`.
#[test]
fn a_host_that_holds_a_lease_is_placed_by_its_latest_hello() {
    let told = Arc::new(Shapes::default());
    let (registry, mut provider, clock) = told_by(by_hello(), &told);
    say(&registry, &hello_of("a", "arm64"));
    assert_eq!(started_on(&provider.start(spec(1))), Some("a"));
    told.said();
    clock.advance(std::time::Duration::from_millis(
        js_agent_wire::DEFAULT_GRACE_MS + 1,
    ));
    say(&registry, &hello_of("a", "x86_64"));
    assert_eq!(told.said(), ["up a 4000 arch=x86_64,os=macos"]);
    let listed = provider.capacity().remove(0);
    assert_eq!(
        listed.shape.capabilities(),
        &map(&[("arch", "x86_64"), ("os", "macos")])
    );
    assert_eq!(listed.leased_to, Some(lease(1)));
}

/// Catches: a host put back with its lease after a restart (listed with the pool's floor
/// alone, which names no `arch`) left on the floor once its agent says hello, so that it is
/// no node of js-sched for as long as it holds the lease and until it says hello again.
/// The lease file puts back a lease on `a`, which then says hello reporting `arm64`: the
/// book lists it with `arch` and the lease, and the placer is told so.
#[test]
fn a_host_put_back_with_a_lease_is_placed_by_its_first_hello() {
    let told = Arc::new(Shapes::default());
    let (registry, provider, _) = told_by(by_hello(), &told);
    assert_eq!(provider.seat_booked(&booking("a", 1, None), None), Ok(()));
    assert_eq!(provider.capacity()[0].shape, floor());
    say(&registry, &hello_of("a", "arm64"));
    assert_eq!(told.said(), ["up a 4000 arch=arm64,os=macos"]);
    let listed = provider.capacity().remove(0);
    assert_eq!(
        listed.shape.capabilities(),
        &map(&[("arch", "arm64"), ("os", "macos")])
    );
    assert_eq!(listed.leased_to, Some(lease(1)));
}

/// Catches: a host of a pool that places by hello whose agent reports no `arch` left out
/// of placement altogether, or placed by js-sched as some architecture. Such a host is no
/// node of the scheduler, as a host of any shape the scheduler cannot describe is: a lease
/// that needs no `arch` is booked on it by the fallback to the first host with room, which
/// is counted, and a lease that needs an `arch` is not booked on it.
#[test]
fn a_host_that_reports_no_arch_is_placed_by_the_counted_fallback() {
    let fx = fixture_of(by_hello());
    let mut said = hello("a");
    said.capabilities.remove("arch");
    fx.hello(&said, &cred("t")).unwrap();
    let mut provider = fx.provider.clone();
    let needs = provider.start(needing(1, &[("arch", "x86_64")]));
    assert_eq!(needs, StartAck::Refused(RefusalCode::NoCapacity));
    assert_eq!(started_on(&provider.start(spec(2))), Some("a"));
    assert_eq!(fx.placer.fallbacks().unmappable, 1);
}

/// Catches: an agent or joined pool (one that does not place by hello) placing by what a
/// hello reported beyond the pool's shape. Its host reports `arch`, which its shape does
/// not name: a lease that needs that `arch` is refused `no-fit`, and a lease that needs only
/// what the shape names is booked on the host.
#[test]
fn a_pool_that_does_not_place_by_hello_places_by_its_shape_alone() {
    let name = ProviderId::new("builders").unwrap();
    let shape = Shape::new(floor().resources().clone(), map(&[("os", "macos")]));
    let fx = fixture_of(PoolConfig::new(name, class(), shape, 4));
    fx.hello(&hello_of("a", "x86_64"), &cred("t")).unwrap();
    let mut provider = fx.provider.clone();
    let needs = provider.start(needing(1, &[("arch", "x86_64")]));
    assert_eq!(needs, StartAck::Refused(RefusalCode::NoFit));
    let plain = provider.start(needing(2, &[("os", "macos")]));
    assert_eq!(started_on(&plain), Some("a"));
}

/// Catches: a pool that places by hello still holding a request to the whole floor (a
/// request that names `arch` would be refused `no-fit` by a floor that names none, and no
/// elastic host would ever take work that needs an architecture), or one that no longer
/// holds it to what the floor does name (resources, and a key the floor names with
/// another value). With an `arm64` host and an `x86_64` host listed, a lease
/// that needs `x86_64` is booked on the `x86_64` host though the `arm64` one comes first
/// by id, one that needs `arm64` (written `aarch64`, the other spelling) on the `arm64`
/// one, and one that needs either again while both are held is refused `no-capacity` (a
/// host could free); a lease above the floor's cores, one that names `os` other than the
/// floor's, and one that needs an accelerator the floor has none of, are refused `no-fit`. The engine agrees with the book throughout, so it is
/// never rebuilt at a start (it would be at each one if it had been told the pool's shape at
/// the hellos while the book lists the hosts' own).
#[test]
fn each_host_takes_the_leases_that_need_the_arch_it_reported() {
    let name = ProviderId::new("builders").unwrap();
    let shape = Shape::new(floor().resources().clone(), map(&[("os", "macos")]));
    let config = PoolConfig::new(name, class(), shape, 4).placing_by_hello();
    let fx = fixture_of(config);
    fx.hello(&hello_of("a", "arm64"), &cred("t")).unwrap();
    fx.hello(&hello_of("x", "x86_64"), &cred("t")).unwrap();
    let mut provider = fx.provider.clone();
    let x86 = provider.start(needing(1, &[("arch", "x86_64")]));
    assert_eq!(started_on(&x86), Some("x"));
    let arm = provider.start(needing(2, &[("arch", "aarch64")]));
    assert_eq!(started_on(&arm), Some("a"));
    let again = provider.start(needing(3, &[("arch", "x86_64")]));
    assert_eq!(again, StartAck::Refused(RefusalCode::NoCapacity));
    let large = Request {
        resources: Resources::new(4001, 1, 0, None).unwrap(),
        ..request()
    };
    let large = StartSpec::new(lease(4), large, &[class()]).unwrap();
    assert_eq!(provider.start(large), StartAck::Refused(RefusalCode::NoFit));
    let linux = provider.start(needing(5, &[("os", "linux"), ("arch", "x86_64")]));
    assert_eq!(linux, StartAck::Refused(RefusalCode::NoFit));
    let gpu = provider.start(needing(6, &[("accelerator.model", "nvidia-a100")]));
    assert_eq!(gpu, StartAck::Refused(RefusalCode::NoFit));
    assert_eq!(fx.placer.resynced(), 0);
}

/// Catches: an elastic host booked for work that needs an architecture it did not report
/// (an `x86_64` machine handed an `arm64` job), or such work refused as if no host could
/// ever serve it. With one `x86_64` host listed and free, a lease that
/// needs `arm64` is not booked on it: it is refused `no-capacity`, since an `arm64` host
/// may still join; once one has said hello, the same lease is booked there. A lease that
/// names an architecture no host reported is refused the same way, and the free `x86_64`
/// host still takes the lease that needs it.
#[test]
fn a_host_takes_no_lease_that_needs_an_arch_it_did_not_report() {
    let fx = fixture_of(by_hello());
    fx.hello(&hello_of("x", "x86_64"), &cred("t")).unwrap();
    let mut provider = fx.provider.clone();
    let arm = provider.start(needing(1, &[("arch", "arm64")]));
    assert_eq!(arm, StartAck::Refused(RefusalCode::NoCapacity));
    fx.hello(&hello_of("b", "arm64"), &cred("t")).unwrap();
    let arm = provider.start(needing(2, &[("arch", "arm64")]));
    assert_eq!(started_on(&arm), Some("b"));
    let riscv = provider.start(needing(3, &[("arch", "riscv64")]));
    assert_eq!(riscv, StartAck::Refused(RefusalCode::NoCapacity));
    let x86 = provider.start(needing(4, &[("arch", "x86_64")]));
    assert_eq!(started_on(&x86), Some("x"));
}

/// Catches: admission reading a host's facts without the capabilities it is placed by (a
/// request for its `arch` would be judged as if no listed host could serve it), or reading
/// them in a pool that does not place by hello. In the host listing an `arm64` host of a
/// pool that places by hello is placed on the floor with `arch` and `os` from its hello;
/// the same host of a pool without the setting is placed on the floor alone, and both list
/// the floor as the shape the pool books.
#[test]
fn the_listing_places_a_host_by_its_hello_only_in_a_pool_that_places_by_it() {
    let view = |config: PoolConfig| {
        let fx = fixture_of(config);
        fx.hello(&hello_of("a", "arm64"), &cred("t")).unwrap();
        let shown = fx.registry.hosts().remove(0);
        assert_eq!(shown.shape, floor());
        (shown.by_hello, shown.placement_shape())
    };
    let (by, placed) = view(by_hello());
    assert!(by);
    let caps = map(&[("arch", "arm64"), ("os", "macos")]);
    assert_eq!(placed, Shape::new(floor().resources().clone(), caps));
    let name = ProviderId::new("builders").unwrap();
    let (by, placed) = view(PoolConfig::new(name, class(), floor(), 4));
    assert!(!by);
    assert_eq!(placed, floor());
}

/// Catches: a placer told the pool floor's accelerator in place of the host's validated
/// one (a host of `nvidia-a100-sxm4-80gb` in a pool whose floor names the family
/// `nvidia-a100` would be a node of the family with no memory known, and js-sched would
/// place no request for the variant on it). In a pool that does not place by hello, the
/// placer is told exactly the shape the book lists: the floor's numbers and `os`, with the
/// accelerator facts of the hello, 80 GiB per device among them.
#[test]
fn the_placer_is_told_the_hosts_accelerator_facts_in_any_pool() {
    let gpu = Resources::new(4000, 8 << 30, 1, Some("nvidia-a100".into())).unwrap();
    let name = ProviderId::new("builders").unwrap();
    let shape = Shape::new(gpu, map(&[("os", "macos")]));
    let told = Arc::new(Shapes::default());
    let (registry, provider, _) = told_by(PoolConfig::new(name, class(), shape, 4), &told);
    let a100_80 = "nvidia-a100-sxm4-80gb";
    let mut said = hello("gpu-1");
    said.shape.gpus = 1;
    said.shape.gpu_class = Some(a100_80.into());
    said.gpu_devices = vec![js_agent_wire::GpuDevice {
        index: 0,
        uuid: "GPU-1".into(),
        model: a100_80.into(),
        memory_bytes: 80 << 30,
    }];
    say(&registry, &said);
    let listed = provider.capacity().remove(0).shape;
    let caps: Vec<String> = listed
        .capabilities()
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect();
    assert!(
        caps.contains(&"accelerator.memory-gib=80".to_owned()),
        "{caps:?}"
    );
    assert_eq!(told.said(), [format!("up gpu-1 4000 {}", caps.join(","))]);
}
