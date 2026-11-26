//! A host's capacity is what it reports: a hello is never refused for reporting less or more
//! CPU, memory, disk or GPUs than the pool's `[shape]`, and the host is booked on each at
//! what it reports (GPUs only where the host can serve them). Only its capabilities (and
//! the class of the GPUs it is booked) are held to the pool's.

mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use js_agent_wire::{Hello, Phase};
use js_registry::{Adopted, Binding, NodeSession, Placer, PoolConfig, Refusal, ShapeFault};
use js_types::{ExecMode, HostId, ProviderId, Resources, Shape};
use support::*;

const GIB: u64 = 1 << 30;

/// The memory the pool's `[shape]` lists: 250 GiB, as `memory_bytes = 268435456000`.
const POOL_MEMORY: u64 = 250 * GIB;

/// The memory a host reports once it has lost a DIMM: 240 GiB.
const REPORTED_MEMORY: u64 = 240 * GIB;

/// A placer that writes down each shape a host is listed with, in order.
#[derive(Default)]
struct Told {
    shapes: Mutex<Vec<(String, Shape)>>,
}

impl Told {
    /// The memory of every shape the placer was told, in order.
    fn memory(&self) -> Vec<u64> {
        let shapes = self.shapes.lock().unwrap();
        shapes
            .iter()
            .map(|(_, shape)| shape.resources().memory_bytes())
            .collect()
    }

    /// The last shape the placer was told for `host`.
    fn last(&self, host: &str) -> Resources {
        let shapes = self.shapes.lock().unwrap();
        let (_, shape) = shapes.iter().rev().find(|(id, _)| id == host).unwrap();
        shape.resources().clone()
    }
}

impl Placer for Told {
    fn host_up(&self, host: &HostId, shape: &Shape) {
        let entry = (host.as_str().to_owned(), shape.clone());
        self.shapes.lock().unwrap().push(entry);
    }
}

/// A macOS pool on [`ARCH`] of 64 cores, [`POOL_MEMORY`], 4 GPUs of class `nvidia-a100` and
/// 500 GiB of disk.
fn big_pool() -> PoolConfig {
    let resources = Resources::new(64_000, POOL_MEMORY, 4, Some("nvidia-a100".into()))
        .unwrap()
        .with_disk_bytes(500 * GIB);
    let shape = Shape::new(resources, map(&[("os", "macos"), ("arch", ARCH)]));
    PoolConfig::new(ProviderId::new("builders").unwrap(), class(), shape, 8)
}

/// A hello for `host` reporting `cpu_millis`, `memory_bytes`, `gpus` of class
/// `nvidia-a100` and `disk_bytes`, with the pool's capabilities.
fn reporting(host: &str, cpu_millis: u64, memory_bytes: u64, gpus: u64, disk_bytes: u64) -> Hello {
    let mut claim = hello(host);
    claim.shape.cpu_millis = cpu_millis;
    claim.shape.memory_bytes = memory_bytes;
    claim.shape.gpus = gpus;
    claim.shape.gpu_class = (gpus > 0).then(|| "nvidia-a100".to_owned());
    claim.shape.disk_bytes = disk_bytes;
    claim
}

/// The resources `fx` lists `host` at ([`js_registry::HostView::shape`]).
fn listed(fx: &Fixture, host: &str) -> Resources {
    let hosts = fx.registry.hosts();
    let view = hosts.iter().find(|view| view.id.as_str() == host).unwrap();
    view.shape.resources().clone()
}

/// Asserts that a host reporting 240 GiB in a pool whose `[shape]` says 250 GiB is admitted
/// and booked against 240 GiB: the placer is told 240 GiB, the listing shows 240 GiB as its
/// shape, its memory headroom and the shape it is placed on, and its claim is kept. It
/// catches the floor refusing the host `below-floor` (a node lost whole for a lost DIMM),
/// and a host admitted but still booked at the pool's 250 GiB, which would place work on
/// memory it does not have.
#[test]
fn a_host_reporting_240_gib_in_a_250_gib_pool_is_admitted_and_booked_at_240() {
    assert_eq!(POOL_MEMORY, 268_435_456_000);
    let told = Arc::new(Told::default());
    let fx = with_placer(big_pool(), told.clone());
    let node = reporting("node-01", 64_000, REPORTED_MEMORY, 4, 500 * GIB);
    fx.hello(&node, &cred("t")).unwrap();
    assert_eq!(told.memory(), [REPORTED_MEMORY]);
    let hosts = fx.registry.hosts();
    let view = &hosts[0];
    assert_eq!(view.shape.resources().memory_bytes(), REPORTED_MEMORY);
    assert_eq!(view.memory.configured_bytes(), REPORTED_MEMORY);
    assert_eq!(view.memory.bookable_bytes(), REPORTED_MEMORY);
    let placed = view.placement_shape();
    assert_eq!(placed.resources().memory_bytes(), REPORTED_MEMORY);
    let claim = view.claim.as_ref().unwrap();
    assert_eq!(claim.resources().memory_bytes(), REPORTED_MEMORY);
}

/// Asserts that each of CPU, memory, GPUs and disk is booked at what the host reports, one
/// dimension at a time and all at once, and that a host reporting more than the shape on
/// every one is booked at its report. It catches a booking clamped to the pool's shape on
/// one dimension only, a booking that ignores a larger claim, and a GPU class dropped while
/// GPUs remain.
#[test]
fn each_count_is_booked_at_what_the_host_reports() {
    let told = Arc::new(Told::default());
    let fx = with_placer(big_pool(), told.clone());
    let more = (128_000, 1024 * GIB, 8, 2000 * GIB);
    let cases = [
        (
            "cpu",
            (48_000, POOL_MEMORY, 4, 500 * GIB),
            (48_000, POOL_MEMORY, 4, 500 * GIB),
        ),
        (
            "gpus",
            (64_000, POOL_MEMORY, 2, 500 * GIB),
            (64_000, POOL_MEMORY, 2, 500 * GIB),
        ),
        (
            "disk",
            (64_000, POOL_MEMORY, 4, 100 * GIB),
            (64_000, POOL_MEMORY, 4, 100 * GIB),
        ),
        ("all", (8000, 16 * GIB, 1, GIB), (8000, 16 * GIB, 1, GIB)),
        ("more", more, more),
    ];
    for (name, (cpu, memory, gpus, disk), booked) in cases {
        fx.hello(&reporting(name, cpu, memory, gpus, disk), &cred("t"))
            .unwrap();
        let resources = listed(&fx, name);
        let counts = (
            resources.cpu_millis(),
            resources.memory_bytes(),
            resources.gpus(),
            resources.disk_bytes(),
        );
        assert_eq!(counts, booked, "{name}");
        assert_eq!(resources.gpu_class(), Some("nvidia-a100"), "{name}");
        assert_eq!(told.last(name), resources, "{name}");
    }
}

/// Asserts that a host reporting more than the pool's `[shape]` on every count (twice the
/// cores, memory and disk, and more GPUs) is admitted and booked at its report: the listing,
/// the memory headroom and the placer all show the larger numbers. It catches a pool shape
/// kept as a ceiling on what a host is booked.
#[test]
fn a_host_reporting_more_than_the_shape_is_booked_at_its_report() {
    let told = Arc::new(Told::default());
    let fx = with_placer(big_pool(), told.clone());
    let node = reporting("big", 128_000, 2 * POOL_MEMORY, 8, 1000 * GIB);
    fx.hello(&node, &cred("t")).unwrap();
    let resources = listed(&fx, "big");
    assert_eq!(resources.cpu_millis(), 128_000);
    assert_eq!(resources.memory_bytes(), 2 * POOL_MEMORY);
    assert_eq!(resources.gpus(), 8);
    assert_eq!(resources.gpu_class(), Some("nvidia-a100"));
    assert_eq!(resources.disk_bytes(), 1000 * GIB);
    assert_eq!(told.last("big"), resources);
    let hosts = fx.registry.hosts();
    assert_eq!(hosts[0].memory.configured_bytes(), 2 * POOL_MEMORY);
    assert_eq!(hosts[0].memory.bookable_bytes(), 2 * POOL_MEMORY);
}

/// Asserts that in a pool with an execution mode (`container`) whose shape has no GPUs, a
/// host reporting 2 GPUs of a class and `runtime=container` with no `gpu-toolkit` joins and
/// is booked no GPUs and no class, while the same host reporting `gpu-toolkit=true` is
/// booked its 2 GPUs of their class. It catches GPUs booked on a host whose containers
/// cannot reach them, and GPUs dropped from a host that can serve them.
#[test]
fn a_container_host_is_booked_its_gpus_only_with_the_gpu_toolkit() {
    let resources = Resources::new(64_000, POOL_MEMORY, 0, None).unwrap();
    let shape = Shape::new(resources, map(&[("os", "macos"), ("arch", ARCH)]));
    let name = ProviderId::new("builders").unwrap();
    let pool = PoolConfig::new(name, class(), shape, 8).with_exec(ExecMode::Container);
    let fx = fixture_of(pool);
    let mut bare = reporting("bare", 64_000, POOL_MEMORY, 2, 0);
    bare.capabilities
        .insert("runtime".into(), "container".into());
    fx.hello(&bare, &cred("t")).unwrap();
    let resources = listed(&fx, "bare");
    assert_eq!(resources.gpus(), 0);
    assert_eq!(resources.gpu_class(), None);
    let mut toolkit = reporting("toolkit", 64_000, POOL_MEMORY, 2, 0);
    toolkit
        .capabilities
        .insert("runtime".into(), "container".into());
    toolkit
        .capabilities
        .insert("gpu-toolkit".into(), "true".into());
    fx.hello(&toolkit, &cred("t")).unwrap();
    let resources = listed(&fx, "toolkit");
    assert_eq!(resources.gpus(), 2);
    assert_eq!(resources.gpu_class(), Some("nvidia-a100"));
}

/// Asserts that a host of a GPU pool reporting no GPU joins and is booked none (with no GPU
/// class), while a host reporting GPUs of another class than the pool's is refused
/// `shape-not-accepted` with the reason `below-floor` and is not listed: the count is an
/// amount, the class a kind. It catches a GPU count held as a floor, and a class no longer
/// compared.
#[test]
fn fewer_gpus_join_and_another_gpu_class_is_refused() {
    let fx = fixture_of(big_pool());
    fx.hello(
        &reporting("cpu-only", 64_000, POOL_MEMORY, 0, 500 * GIB),
        &cred("t"),
    )
    .unwrap();
    let resources = listed(&fx, "cpu-only");
    assert_eq!(resources.gpus(), 0);
    assert_eq!(resources.gpu_class(), None);
    let mut other = reporting("h100", 64_000, POOL_MEMORY, 4, 500 * GIB);
    other.shape.gpu_class = Some("nvidia-h100".into());
    let refused = fx.registry.hello(&other, &cred("t")).unwrap_err();
    assert_eq!(refused, Refusal::ShapeNotAccepted(ShapeFault::BelowFloor));
    assert_eq!(refused.reason(), Some("below-floor"));
    let hosts = fx.registry.hosts();
    assert_eq!(hosts.len(), 1);
    assert_eq!(hosts[0].id.as_str(), "cpu-only");
}

/// Asserts that a host whose report shrinks between two hellos is booked at the new report:
/// a host that joined at the pool's 250 GiB, went silent and said hello again with 240 GiB
/// is listed and told at 240 GiB, and at 250 GiB again once it reports that. It catches a
/// booking kept from the first hello.
#[test]
fn a_host_whose_report_shrinks_is_booked_at_the_new_report() {
    let told = Arc::new(Told::default());
    let fx = with_placer(big_pool(), told.clone());
    let at = |memory| reporting("node-01", 64_000, memory, 4, 500 * GIB);
    fx.hello(&at(POOL_MEMORY), &cred("t")).unwrap();
    fx.pass_grace();
    fx.hello(&at(REPORTED_MEMORY), &cred("t")).unwrap();
    assert_eq!(listed(&fx, "node-01").memory_bytes(), REPORTED_MEMORY);
    fx.pass_grace();
    fx.hello(&at(POOL_MEMORY), &cred("t")).unwrap();
    assert_eq!(listed(&fx, "node-01").memory_bytes(), POOL_MEMORY);
    assert_eq!(told.memory(), [POOL_MEMORY, REPORTED_MEMORY, POOL_MEMORY]);
}

/// Asserts that the shape a stored row is placed on ([`js_registry::Registry::placement_shape_in`])
/// is the row's claim, so a row written before this booking, at the pool's 250 GiB, places
/// a host that claimed 240 GiB at 240 GiB and one that claimed 500 GiB at 500 GiB; a row
/// with no claim keeps what it booked; and a pool the registry does not serve has none. It
/// catches the row path still placing at the pool's numbers while the hello path books the
/// claim.
#[test]
fn a_row_booked_at_the_pool_shape_is_placed_at_its_claim() {
    let fx = fixture_of(big_pool());
    let booked = big_pool().shape().clone();
    let claim = reporting("node-01", 64_000, REPORTED_MEMORY, 4, 500 * GIB)
        .to_shape()
        .unwrap();
    let placed = fx
        .registry
        .placement_shape_in("builders", &booked, Some(&claim))
        .unwrap();
    assert_eq!(placed.resources().memory_bytes(), REPORTED_MEMORY);
    let larger = reporting("node-02", 96_000, 500 * GIB, 4, 500 * GIB)
        .to_shape()
        .unwrap();
    let placed = fx
        .registry
        .placement_shape_in("builders", &booked, Some(&larger))
        .unwrap();
    assert_eq!(placed.resources().memory_bytes(), 500 * GIB);
    assert_eq!(placed.resources().cpu_millis(), 96_000);
    let unclaimed = fx.registry.placement_shape_in("builders", &booked, None);
    assert_eq!(unclaimed.unwrap().resources(), booked.resources());
    assert_eq!(fx.registry.placement_shape_in("other", &booked, None), None);
}

/// Asserts that a session another server's hello wrote, adopted from its row, is booked at
/// the claim the row keeps, below or above the pool's shape: the listing and the placer
/// show 240 GiB (or 500 GiB for a larger claim), as a hello here would. A row that keeps no
/// claim is booked at the pool's shape. It catches the adopt path booking the pool's numbers while the hello path books
/// the claim.
#[test]
fn an_adopted_session_is_booked_at_the_claim_its_row_keeps() {
    let told = Arc::new(Told::default());
    let fx = with_placer(big_pool(), told.clone());
    let session = |claim: Option<Shape>| NodeSession {
        epoch: 1,
        session: None,
        pool: "builders".into(),
        claim,
        labels: map(&[]),
        phase: Phase::Serving,
        agent_process: None,
        agent_version: None,
        silent_for: Duration::ZERO,
        binding: Some(Binding::new(cred("t"), None)),
    };
    let claim = reporting("node-01", 64_000, REPORTED_MEMORY, 4, 500 * GIB)
        .to_shape()
        .unwrap();
    let adopted = fx.registry.adopt(&host("node-01"), session(Some(claim)));
    assert_eq!(adopted, Adopted::Installed);
    assert_eq!(listed(&fx, "node-01").memory_bytes(), REPORTED_MEMORY);
    assert_eq!(told.last("node-01").memory_bytes(), REPORTED_MEMORY);
    let larger = reporting("node-02", 96_000, 500 * GIB, 4, 500 * GIB)
        .to_shape()
        .unwrap();
    let adopted = fx.registry.adopt(&host("node-02"), session(Some(larger)));
    assert_eq!(adopted, Adopted::Installed);
    assert_eq!(listed(&fx, "node-02").memory_bytes(), 500 * GIB);
    assert_eq!(told.last("node-02").cpu_millis(), 96_000);
    let adopted = fx.registry.adopt(&host("unclaimed"), session(None));
    assert_eq!(adopted, Adopted::Installed);
    assert_eq!(listed(&fx, "unclaimed"), *big_pool().shape().resources());
}

/// Asserts that a host put back after a restart (a tainted one), before its hello, is
/// listed at the pool's shape (it has reported nothing), and that its first hello reporting
/// 240 GiB books it at 240 GiB. It catches a restored host whose booking is never replaced
/// by what it reports.
#[test]
fn a_restored_host_is_booked_at_what_its_first_hello_reports() {
    let fx = fixture_of(big_pool());
    let taint = js_agent_wire::Taint {
        reason: "restored".into(),
        run_id: None,
        check: None,
    };
    let binding = Binding::new(cred("t"), None).with_taint(Some(taint));
    fx.registry.restore_tainted(&host("node-01"), binding);
    assert_eq!(listed(&fx, "node-01").memory_bytes(), POOL_MEMORY);
    let node = reporting("node-01", 64_000, REPORTED_MEMORY, 4, 500 * GIB);
    fx.hello(&node, &cred("t")).unwrap();
    assert_eq!(listed(&fx, "node-01").memory_bytes(), REPORTED_MEMORY);
}
