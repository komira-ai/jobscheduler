//! Disk as an admitted resource: what the pool's `disk_bytes` floors at a hello, and which
//! hosts the quota rules admit. Nothing here enforces a quota; the agent does, and these are
//! the server's rules at admission.

mod support;

use js_agent_wire::Hello;
use js_caps::{SCRATCH_DEFAULT_GIB, SCRATCH_QUOTA, SCRATCH_QUOTA_PROJECT};
use js_registry::{PoolConfig, Refusal};
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
