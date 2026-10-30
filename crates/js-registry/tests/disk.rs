//! Disk as an allocated resource: what the pool's `disk_bytes` floors and books, what a
//! run without its own `disk_bytes` is booked, and which hosts may take a job that sets
//! one. Nothing here enforces a quota; the agent does, and these are the server's sums.

mod support;

use js_agent_wire::{HeldRun, HeldState, Hello, Isolation, JobSpec, Limits, Outcome, RunResult};
use js_caps::{SCRATCH_DEFAULT_GIB, SCRATCH_QUOTA, SCRATCH_QUOTA_PROJECT};
use js_provider::Provider;
use js_registry::{PoolConfig, Refusal, RunRefusal};
use js_types::{ProviderId, Resources, Shape};
use support::*;

const GIB: u64 = 1 << 30;

/// A pool of `disk` bytes of disk per host, 4 cores and 8 GiB, which names
/// `scratch-quota` when `quota` is set (so a host must report it to join).
fn pool(disk: u64, quota: bool) -> PoolConfig {
    let resources = Resources::new(4000, 8 << 30, 0, None)
        .unwrap()
        .with_disk_bytes(disk);
    let mut caps = map(&[("os", "macos")]);
    if quota {
        caps.insert(SCRATCH_QUOTA.into(), SCRATCH_QUOTA_PROJECT.into());
    }
    let name = ProviderId::new("builders").unwrap();
    PoolConfig::new(name, class(), Shape::new(resources, caps), 4)
}

/// A hello offering `disk` bytes, reporting the quota capability with the default of
/// `default_gib` GiB when given, and listing `held` runs as running.
fn host_hello(disk: u64, quota: Option<&str>, default_gib: Option<&str>, held: &[&str]) -> Hello {
    let mut hello = hello("a");
    hello.shape.disk_bytes = disk;
    if let Some(value) = quota {
        hello
            .capabilities
            .insert(SCRATCH_QUOTA.into(), value.into());
    }
    if let Some(value) = default_gib {
        hello
            .capabilities
            .insert(SCRATCH_DEFAULT_GIB.into(), value.into());
    }
    hello.held = held
        .iter()
        .map(|run| js_agent_wire::HeldRun {
            run_id: (*run).into(),
            state: js_agent_wire::HeldState::Running,
        })
        .collect();
    hello
}

fn job(run: &str, disk: Option<u64>) -> JobSpec {
    JobSpec {
        run_id: run.into(),
        lease_id: String::new(),
        image: "busybox:1@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
            .into(),
        native: None,
        bundle: None,
        args: Vec::new(),
        limits: Limits {
            cpu_millis: 1000,
            memory_bytes: 1 << 20,
            disk_bytes: disk,
            pids: None,
            wall_ms: 60_000,
        },
        gpu_indices: Vec::new(),
        mounts: Vec::new(),
        files: Vec::new(),
        isolation: Isolation::Contained,
        labels: Default::default(),
        heartbeat_seq: 0,
        valid_for_ms: 0,
    }
}

/// A fixture over `config` with host `a` joined by `hello` and leased by lease 1.
fn leased(config: PoolConfig, hello: &Hello) -> Fixture {
    let fx = fixture_of(config);
    fx.hello(hello, &cred("t")).unwrap();
    assert!(started_on(&fx.provider.clone().start(spec(1))).is_some());
    fx
}

fn queue(fx: &Fixture, run: &str, disk: Option<u64>) -> Result<(), RunRefusal> {
    let queued = fx.registry.queue_run(&host("a"), &lease(1), job(run, disk));
    queued.map(|_| ())
}

/// Asserts that a run with no `disk_bytes` is booked the host's default quota and one
/// with `disk_bytes` its own, summed over the host's live runs against the pool's disk:
/// with a 25 GiB pool and a 10 GiB default, a run without disk (10) and a run of 15 GiB
/// fit, and one more byte is refused `no-capacity`. It catches a sum that books nothing
/// for a run without disk, or that books the default for a run that named its own.
#[test]
fn a_run_without_disk_books_the_host_default() {
    let hello = host_hello(25 * GIB, Some("project"), Some("10"), &[]);
    let fx = leased(pool(25 * GIB, true), &hello);
    assert_eq!(queue(&fx, "r1", None), Ok(()));
    assert_eq!(queue(&fx, "r2", Some(15 * GIB)), Ok(()));
    assert_eq!(queue(&fx, "r3", Some(1)), Err(RunRefusal::NoCapacity));
    assert_eq!(queue(&fx, "r4", None), Err(RunRefusal::NoCapacity));
}

/// Asserts that a change of the host's default (a later hello) does not change what a
/// live run holds: two runs booked at a 10 GiB default hold 20 GiB of a 25 GiB pool after
/// the host says hello again with a 5 GiB default; one new run (5) then fits and the next
/// is refused. A server that re-read the default for runs already queued would count the
/// two old runs at 5 and admit it.
#[test]
fn default_change_does_not_rebook_live_run() {
    let first = host_hello(25 * GIB, Some("project"), Some("10"), &[]);
    let fx = leased(pool(25 * GIB, true), &first);
    assert_eq!(queue(&fx, "r1", None), Ok(()));
    assert_eq!(queue(&fx, "r2", None), Ok(()));
    fx.pass_grace();
    let again = host_hello(25 * GIB, Some("project"), Some("5"), &["r1", "r2"]);
    fx.hello(&again, &cred("t")).unwrap();
    assert_eq!(queue(&fx, "r3", None), Ok(()));
    assert_eq!(queue(&fx, "r4", None), Err(RunRefusal::NoCapacity));
}

/// Asserts that a pool with no disk (`disk_bytes` 0) that does not name `scratch-quota`
/// books none for any run of a host that reports no quota: three runs without disk are
/// queued where one default would not fit zero (a host that reports a quota is refused at
/// the hello, see `the_hello_floor_includes_disk`). A job that sets
/// its own `disk_bytes` is refused `limits-exceed-shape`, since the pool books none. It
/// catches a booking of the default against a shape of zero, which would refuse every run
/// of an existing pool.
#[test]
fn non_quota_pool_zero_disk_books_none() {
    let hello = host_hello(0, None, None, &[]);
    let fx = leased(pool(0, false), &hello);
    for run in ["r1", "r2", "r3"] {
        assert_eq!(queue(&fx, run, None), Ok(()), "{run}");
    }
    assert_eq!(
        queue(&fx, "r4", Some(1)),
        Err(RunRefusal::LimitsExceedShape)
    );
}

/// Asserts that a job that sets `disk_bytes` is queued only for a host that reported
/// `scratch-quota` as `project` with a default that counts, and is otherwise refused
/// `disk-unsupported` with nothing queued: no capability, another value, no default, a
/// default that is not a number, zero, or too large to count. A job without `disk_bytes`
/// is queued for each of them. It catches a server that hands a disk bound to an agent
/// that would ignore it.
#[test]
fn a_disk_job_goes_only_to_a_host_with_a_quota() {
    let huge = u64::MAX.to_string();
    let cases = [
        (None, None),
        (Some("project"), None),
        (None, Some("10")),
        (Some("yes"), Some("10")),
        (Some("project"), Some("ten")),
        (Some("project"), Some("0")),
        (Some("project"), Some(huge.as_str())),
    ];
    for (quota, default) in cases {
        let hello = host_hello(25 * GIB, quota, default, &[]);
        let fx = leased(pool(25 * GIB, false), &hello);
        let refused = queue(&fx, "r1", Some(GIB));
        assert_eq!(
            refused,
            Err(RunRefusal::DiskUnsupported),
            "{quota:?} {default:?}"
        );
        assert!(fx.registry.runs_of(&lease(1)).is_empty());
        assert_eq!(queue(&fx, "r2", None), Ok(()), "{quota:?} {default:?}");
    }
    assert_eq!(RunRefusal::DiskUnsupported.code(), "disk-unsupported");
    let hello = host_hello(25 * GIB, Some("project"), Some("10"), &[]);
    let fx = leased(pool(25 * GIB, false), &hello);
    assert_eq!(queue(&fx, "r1", Some(GIB)), Ok(()));
}

/// Asserts that a job asking for more disk than the pool books is refused
/// `limits-exceed-shape` (the 422 that is never retried) before any host state is read,
/// and that one asking for exactly the pool's disk is queued. It catches a job that
/// could never fit being left to wait for room that cannot come.
#[test]
fn a_job_larger_than_the_pool_disk_exceeds_the_shape() {
    let hello = host_hello(25 * GIB, Some("project"), Some("10"), &[]);
    let fx = leased(pool(25 * GIB, true), &hello);
    assert_eq!(
        queue(&fx, "r1", Some(25 * GIB + 1)),
        Err(RunRefusal::LimitsExceedShape)
    );
    assert_eq!(queue(&fx, "r2", Some(25 * GIB)), Ok(()));
}

/// Asserts the hello floor on disk: a host that offers less disk than the pool books is
/// refused `shape-not-accepted`, as is a host from before the field (zero disk), while a
/// host that offers the pool's disk or more joins, and a pool with no disk keeps
/// accepting hosts with none. It catches a floor that ignores disk, which would admit a
/// host with less disk than every run is booked against.
#[test]
fn the_hello_floor_includes_disk() {
    let join = |config: PoolConfig, disk: u64| {
        let fx = fixture_of(config);
        let quota = Some("project");
        fx.hello(&host_hello(disk, quota, Some("10"), &[]), &cred("t"))
    };
    let refused = join(pool(25 * GIB, true), 25 * GIB - 1).unwrap_err();
    assert_eq!(refused, Refusal::ShapeNotAccepted);
    let legacy = join(pool(25 * GIB, true), 0).unwrap_err();
    assert_eq!(legacy, Refusal::ShapeNotAccepted);
    assert!(join(pool(25 * GIB, true), 25 * GIB).is_ok());
    assert!(join(pool(25 * GIB, true), 30 * GIB).is_ok());
    // A pool that books no disk and names no quota joins a host that reports none, and
    // refuses one that reports a quota (the pool would book 0 where the host books its
    // default).
    let plain = fixture_of(pool(0, false));
    assert!(
        plain
            .hello(&host_hello(0, None, None, &[]), &cred("t"))
            .is_ok()
    );
    assert_eq!(
        join(pool(0, false), 0).unwrap_err(),
        Refusal::ShapeNotAccepted
    );
    // A quota that reports no usable default books nothing on either side.
    let unusable = fixture_of(pool(0, false));
    let hello = host_hello(0, Some("project"), Some("0"), &[]);
    assert!(unusable.hello(&hello, &cred("t")).is_ok());
}

/// Asserts that a quota pool built in code with no disk is refused by `checked`, as the
/// loaders do, with the error that names the cause; and that a pool that does not name
/// the capability, or books disk, passes unchanged. It catches the rule being dropped from
/// the shared check the file loader relies on.
#[test]
fn checked_refuses_a_quota_pool_without_disk() {
    use js_registry::ConfigError;
    assert_eq!(
        pool(0, true).checked().unwrap_err(),
        ConfigError::QuotaWithoutDisk
    );
    assert_eq!(pool(0, false).checked(), Ok(pool(0, false)));
    assert_eq!(pool(GIB, true).checked(), Ok(pool(GIB, true)));
    assert_eq!(pool(GIB, false).checked(), Ok(pool(GIB, false)));
}

/// Asserts that a quota pool refuses, at hello, a host whose default cannot be booked:
/// the default larger than the pool's disk (26 GiB in a 25 GiB pool, and 2^33 GiB, which
/// `checked_mul` still counts), no default, zero, not a number, too large to count, or a
/// quota that is not `project`; all `shape-not-accepted`. A default equal to the pool's
/// disk joins. It catches a host that joins and then fails every run without disk
/// `no-capacity` forever, and a pool that books zero for runs without disk.
#[test]
fn a_quota_pool_refuses_a_host_without_a_default_that_fits() {
    let join = |quota: Option<&str>, default: Option<&str>| {
        let fx = fixture_of(pool(25 * GIB, true));
        fx.hello(&host_hello(25 * GIB, quota, default, &[]), &cred("t"))
    };
    let huge = u64::MAX.to_string();
    let refused = [
        (Some("project"), Some("26")),
        (Some("project"), Some("8589934592")),
        (Some("project"), Some(huge.as_str())),
        (Some("project"), Some("0")),
        (Some("project"), Some("ten")),
        (Some("project"), None),
        (Some("yes"), Some("10")),
        (None, Some("10")),
        (None, None),
    ];
    for (quota, default) in refused {
        let joined = join(quota, default);
        assert_eq!(
            joined.map(|_| ()),
            Err(Refusal::ShapeNotAccepted),
            "{quota:?} {default:?}"
        );
    }
    assert!(join(Some("project"), Some("25")).is_ok());
    assert!(join(Some("project"), Some("1")).is_ok());
}

fn listed(runs: &[&str]) -> Vec<HeldRun> {
    let run = |id: &&str| HeldRun {
        run_id: (*id).into(),
        state: HeldState::Running,
    };
    runs.iter().map(run).collect()
}

fn beat(fx: &Fixture, epoch: u64, held: &[&str], done: &[&str]) {
    let mut message = fx.heartbeat("a", epoch);
    message.held = listed(held);
    message.results = done
        .iter()
        .map(|id| RunResult {
            run_id: (*id).into(),
            lease_id: lease(1).to_string(),
            outcome: Outcome::Succeeded,
            exit_code: Some(0),
            detail: None,
            log_tail: None,
        })
        .collect();
    fx.registry.heartbeat(&message, &cred("t")).unwrap();
}

/// Asserts that a run that ends gives its booked disk back, whichever way it ends: a
/// 20 GiB run of a 25 GiB pool leaves only 5 GiB, a second 10 GiB run is refused
/// `no-capacity`, and after the first finishes (its result sent and no longer listed),
/// is cancelled before the host listed it, or is lost with its host, the second is
/// queued. Meanwhile a run still listed keeps holding. It catches a sum over every run
/// the registry remembers instead of the live ones.
#[test]
fn an_ended_run_frees_its_disk() {
    type End = fn(&Fixture, u64);
    let ends: [(&str, End); 3] = [
        ("finished", |fx, epoch| {
            beat(fx, epoch, &["r1"], &[]);
            beat(fx, epoch, &[], &["r1"]);
        }),
        ("cancelled", |fx, epoch| {
            fx.registry.stop_run(&lease(1), "r1").unwrap();
            // The first beat carries the stop out; the second finds nothing listed.
            beat(fx, epoch, &[], &[]);
            beat(fx, epoch, &[], &[]);
        }),
        ("lost", |fx, epoch| {
            fx.pass_grace();
            beat(fx, epoch, &[], &[]);
        }),
    ];
    for (how, end) in ends {
        let hello = host_hello(25 * GIB, Some("project"), Some("10"), &[]);
        let fx = fixture_of(pool(25 * GIB, true));
        let epoch = fx.hello(&hello, &cred("t")).unwrap().epoch;
        assert!(started_on(&fx.provider.clone().start(spec(1))).is_some());
        assert_eq!(queue(&fx, "r1", Some(20 * GIB)), Ok(()), "{how}");
        assert_eq!(
            queue(&fx, "r2", Some(10 * GIB)),
            Err(RunRefusal::NoCapacity)
        );
        end(&fx, epoch);
        assert_eq!(queue(&fx, "r2", Some(10 * GIB)), Ok(()), "{how}");
        assert_eq!(
            queue(&fx, "r3", Some(16 * GIB)),
            Err(RunRefusal::NoCapacity)
        );
    }
}

/// Asserts that a run the host still lists keeps its disk: after a heartbeat that lists
/// the 20 GiB run, a 10 GiB run is still refused. It catches a release on listing.
#[test]
fn a_listed_run_keeps_its_disk() {
    let hello = host_hello(25 * GIB, Some("project"), Some("10"), &[]);
    let fx = leased(pool(25 * GIB, true), &hello);
    assert_eq!(queue(&fx, "r1", Some(20 * GIB)), Ok(()));
    beat(&fx, 1, &["r1"], &[]);
    assert_eq!(
        queue(&fx, "r2", Some(10 * GIB)),
        Err(RunRefusal::NoCapacity)
    );
}
