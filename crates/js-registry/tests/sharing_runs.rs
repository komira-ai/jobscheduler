//! Runs of leases that share a host, held to what each lease booked, one dimension at a
//! time. The host has 4 cores, 8 GiB, 2 GPUs and 8 GiB of disk, with a 1 GiB default disk
//! quota; leases 1 and 2 each book half of every dimension (2 cores, 4 GiB, 1 GPU, 4 GiB of
//! disk). In each test only the dimension it is about reaches its limit, the others keep
//! room, so the test fails only if that dimension's check is gone. Each runs twice: over
//! leases started on the host, and over leases put back after a restart.

mod support;

use std::sync::Arc;
use std::time::Duration;

use js_agent_wire::{Heartbeat, HeldRun, HeldState, Hello, JobSpec};
use js_caps::{SCRATCH_DEFAULT_GIB, SCRATCH_QUOTA, SCRATCH_QUOTA_PROJECT};
use js_pool_static::Candidate;
use js_provider::{StartAck, StartSpec};
use js_registry::{Placer, PoolConfig, RunRefusal};
use js_types::{Admission, ClassId, Dims, ProviderId, Request, ResourceClass, Resources, Shape};
use support::world::job;
use support::*;

const GIB: u64 = 1 << 30;

/// A placer that books the first host it is shown.
struct First;

impl Placer for First {
    fn pick(&self, _: &StartSpec, _: &[Candidate]) -> Option<usize> {
        Some(0)
    }
}

fn book() -> ResourceClass {
    let id = ClassId::new("book").unwrap();
    ResourceClass::new(id, Admission::Book(Dims::new()), 0)
}

/// Half of the host in every dimension: what each of the two leases books.
fn half() -> Resources {
    Resources::new(2000, 4 * GIB, 1, Some("g".into()))
        .unwrap()
        .with_disk_bytes(4 * GIB)
}

/// The pool: 4 cores, 8 GiB, 2 GPUs of class `g`, 8 GiB of disk, a quota on disk.
fn pool_config() -> PoolConfig {
    let resources = Resources::new(4000, 8 * GIB, 2, Some("g".into()))
        .unwrap()
        .with_disk_bytes(8 * GIB);
    let mut caps = map(&[("os", "macos")]);
    caps.insert(SCRATCH_QUOTA.into(), SCRATCH_QUOTA_PROJECT.into());
    let name = ProviderId::new("builders").unwrap();
    PoolConfig::new(name, book(), Shape::new(resources, caps), 4)
}

/// The hello of host `a`: the pool's shape, a project quota with a 1 GiB default.
fn host_hello() -> Hello {
    let mut hello = hello("a");
    hello.shape.cpu_millis = 4000;
    hello.shape.memory_bytes = 8 * GIB;
    hello.shape.gpus = 2;
    hello.shape.gpu_class = Some("g".into());
    hello.shape.disk_bytes = 8 * GIB;
    hello
        .capabilities
        .insert(SCRATCH_QUOTA.into(), SCRATCH_QUOTA_PROJECT.into());
    hello
        .capabilities
        .insert(SCRATCH_DEFAULT_GIB.into(), "1".into());
    hello
}

fn spec_of(sequence: u64) -> StartSpec {
    let request = Request {
        class: book().id().clone(),
        resources: half(),
        max_wall: Duration::from_secs(600),
        ..request()
    };
    StartSpec::new(lease(sequence), request, &[book()]).unwrap()
}

/// How leases 1 and 2 came to be on `a`.
#[derive(Debug, Clone, Copy)]
enum Came {
    /// Started by the registry, with a placer.
    Started,
    /// Put back after a restart, with what they booked, before the host said hello.
    Restored,
}

/// Host `a` registered, with leases 1 and 2 on it as `came` says, and its session epoch.
fn host_with_two_leases(came: Came) -> (Fixture, u64) {
    let fx = fixture_of(pool_config());
    fx.registry.set_placer(Arc::new(First));
    let mut provider = fx.registry.provider().with_classes(vec![book()]);
    if let Came::Restored = came {
        for n in 1..=2 {
            let seated = provider.seat_booked(&host("a"), lease(n), None, Some(half()));
            assert_eq!(seated, Ok(()));
        }
    }
    let epoch = fx.hello(&host_hello(), &cred("t")).unwrap().epoch;
    if let Came::Started = came {
        for n in 1..=2 {
            let ack = js_provider::Provider::start(&mut provider, spec_of(n));
            assert_eq!(ack, StartAck::Started { host: host("a") });
        }
    }
    (fx, epoch)
}

/// A run named `run`: `cpu_millis`, `memory_bytes`, `disk` (`None` takes the host's
/// default quota) and the GPU indices `gpus`.
fn run(run: &str, cpu_millis: u64, memory_bytes: u64, disk: Option<u64>, gpus: &[u32]) -> JobSpec {
    let mut spec = job(run);
    spec.limits.cpu_millis = cpu_millis;
    spec.limits.memory_bytes = memory_bytes;
    spec.limits.disk_bytes = disk;
    spec.gpu_indices = gpus.to_vec();
    spec
}

fn queue(fx: &Fixture, n: u64, spec: JobSpec) -> Result<(), RunRefusal> {
    fx.registry
        .queue_run(&host("a"), &lease(n), spec)
        .map(|_| ())
}

/// Catches: the cores of a lease's runs not summed against its share. Lease 1's runs take
/// its 2 cores exactly with memory, disk and GPUs to spare, and one millicore more is
/// refused; lease 2 still runs its own 2 cores.
#[test]
fn a_leases_runs_together_are_held_to_its_cores() {
    for came in [Came::Started, Came::Restored] {
        let (fx, _) = host_with_two_leases(came);
        queue(&fx, 1, run("c1", 1500, 1, Some(1), &[])).unwrap();
        queue(&fx, 1, run("c2", 500, 1, Some(1), &[])).unwrap();
        let over = queue(&fx, 1, run("c3", 1, 1, Some(1), &[]));
        assert_eq!(over, Err(RunRefusal::NoCapacity), "{came:?}");
        queue(&fx, 2, run("d1", 2000, 1, Some(1), &[])).unwrap();
    }
}

/// Catches: the memory of a lease's runs not summed against its share. Lease 1's runs take
/// its 4 GiB exactly with cores, disk and GPUs to spare, and one byte more is refused;
/// lease 2 still runs its own 4 GiB.
#[test]
fn a_leases_runs_together_are_held_to_its_memory() {
    for came in [Came::Started, Came::Restored] {
        let (fx, _) = host_with_two_leases(came);
        queue(&fx, 1, run("m1", 1, 3 * GIB, Some(1), &[])).unwrap();
        queue(&fx, 1, run("m2", 1, GIB, Some(1), &[])).unwrap();
        let over = queue(&fx, 1, run("m3", 1, 1, Some(1), &[]));
        assert_eq!(over, Err(RunRefusal::NoCapacity), "{came:?}");
        queue(&fx, 2, run("n1", 1, 4 * GIB, Some(1), &[])).unwrap();
    }
}

/// Catches: the disk of a lease's runs not held to its share. One run asking more than the
/// lease's 4 GiB is refused as past its shape. A run of 3 GiB and one that sets no disk
/// (booked the host's 1 GiB default quota) fill it exactly, and a run of one byte more is
/// refused, with cores, memory and GPUs to spare. Lease 2 still runs its own 4 GiB. When
/// the 3 GiB run ends (listed, then no longer listed), its disk is free again: a 3 GiB run
/// fits and one byte more does not.
#[test]
fn a_leases_runs_together_are_held_to_its_disk() {
    for came in [Came::Started, Came::Restored] {
        let (fx, epoch) = host_with_two_leases(came);
        let big = queue(&fx, 1, run("big", 1, 1, Some(4 * GIB + 1), &[]));
        assert_eq!(big, Err(RunRefusal::LimitsExceedShape), "{came:?}");
        queue(&fx, 1, run("k1", 1, 1, Some(3 * GIB), &[])).unwrap();
        queue(&fx, 1, run("k2", 1, 1, None, &[])).unwrap();
        let over = queue(&fx, 1, run("k3", 1, 1, Some(1), &[]));
        assert_eq!(over, Err(RunRefusal::NoCapacity), "{came:?}");
        queue(&fx, 2, run("l1", 1, 1, Some(4 * GIB), &[])).unwrap();

        let beat = |held: &[&str]| {
            let listed = held.iter().map(|id| HeldRun {
                run_id: (*id).into(),
                state: HeldState::Running,
            });
            let beat = Heartbeat {
                held: listed.collect(),
                ..fx.heartbeat("a", epoch)
            };
            fx.registry.heartbeat(&beat, &cred("t")).unwrap();
        };
        beat(&["k1", "k2", "l1"]);
        beat(&["k2", "l1"]);
        queue(&fx, 1, run("k4", 1, 1, Some(3 * GIB), &[])).unwrap();
        let over = queue(&fx, 1, run("k5", 1, 1, Some(1), &[]));
        assert_eq!(over, Err(RunRefusal::NoCapacity), "{came:?}");
    }
}

/// Catches: the GPUs of a lease's runs not counted against its share, which would let a
/// lease that booked one GPU take both. A run naming both GPUs is refused as past the
/// lease's shape. Lease 1 runs on GPU 0; a second run of it on GPU 1, which clashes with
/// nothing and leaves cores, memory and disk to spare, is refused; lease 2 runs on GPU 1.
#[test]
fn a_leases_runs_together_are_held_to_its_gpus() {
    for came in [Came::Started, Came::Restored] {
        let (fx, _) = host_with_two_leases(came);
        let both = queue(&fx, 1, run("g01", 1, 1, Some(1), &[0, 1]));
        assert_eq!(both, Err(RunRefusal::LimitsExceedShape), "{came:?}");
        queue(&fx, 1, run("g0", 1, 1, Some(1), &[0])).unwrap();
        let second = queue(&fx, 1, run("g1", 1, 1, Some(1), &[1]));
        assert_eq!(second, Err(RunRefusal::NoCapacity), "{came:?}");
        queue(&fx, 2, run("h1", 1, 1, Some(1), &[1])).unwrap();
    }
}
