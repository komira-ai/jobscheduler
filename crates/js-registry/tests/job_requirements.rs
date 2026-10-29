//! Disk and execution mode on a job: a request that requires `exec` or `disk-gib` is
//! placed by what its hosts reported at admission, never by what the pool is called.

mod support;

use js_agent_wire::Hello;
use js_provider::{Provider, RefusalCode, StartAck, StartSpec};
use js_registry::{PoolConfig, Refusal};
use js_types::{ExecMode, ProviderId, Request, Shape};
use support::*;

/// A pool of macOS hosts that declares `exec`.
fn pool(exec: ExecMode) -> Fixture {
    fixture_of(config(4).with_exec(exec))
}

/// A pool whose shape promises at least `disk` GiB free on every host.
fn disk_pool(disk: &str) -> Fixture {
    let shape = Shape::new(
        pool_shape().resources().clone(),
        map(&[("os", "macos"), ("disk-gib", disk)]),
    );
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

/// Asserts that a pool of `sandbox-exec` hosts serves a job that requires `sandbox-exec`
/// or names no mode, and refuses, as one no host could ever serve, a job that requires
/// `container`; and the same the other way round. It catches a mode that is not part of
/// the match, which would send a macOS job to a container host.
#[test]
fn a_job_requiring_a_mode_never_goes_to_the_other_mode() {
    let mut mac = pool(ExecMode::SandboxExec);
    let on_mac = reporting(
        "m",
        &[("runtime", "sandbox-exec"), ("exec", "sandbox-exec")],
    );
    mac.hello(&on_mac, &cred("t")).unwrap();
    let wrong = mac.provider.start(wanting(&[("exec", "container")]));
    assert_eq!(wrong, StartAck::Refused(RefusalCode::NoFit));
    assert!(mac.provider.booked().is_empty());
    let right = mac.provider.start(wanting(&[("exec", "sandbox-exec")]));
    assert_eq!(started_on(&right), Some("m"));

    let mut box_ = pool(ExecMode::Container);
    let in_box = reporting("c", &[("runtime", "container")]);
    box_.hello(&in_box, &cred("t")).unwrap();
    let wrong = box_.provider.start(wanting(&[("exec", "sandbox-exec")]));
    assert_eq!(wrong, StartAck::Refused(RefusalCode::NoFit));
    let either = box_.provider.start(wanting(&[]));
    assert_eq!(started_on(&either), Some("c"));
}

/// Asserts that the mode a pool books is the one its hosts were admitted on: a host that
/// reports the other mode is refused `execution-mismatch`, so no job requiring a mode is
/// ever placed on a host that runs the other.
#[test]
fn the_booked_mode_is_held_to_what_the_host_reports() {
    let mac = pool(ExecMode::SandboxExec);
    let liar = reporting("x", &[("runtime", "container"), ("exec", "container")]);
    let refused = mac.hello(&liar, &cred("t")).unwrap_err();
    assert_eq!(refused, Refusal::ExecutionMismatch { field: "exec" });
    assert!(mac.registry.hosts().is_empty());
}

/// Asserts that a pool that declares no mode books none: a job that requires one finds no
/// host unless the pool's capabilities name it, and a job that names none is served.
#[test]
fn a_pool_without_a_mode_serves_only_jobs_that_ask_for_none() {
    let mut fx = fixture(4);
    fx.join("a", "t");
    let asking = fx.provider.start(wanting(&[("exec", "container")]));
    assert_eq!(asking, StartAck::Refused(RefusalCode::NoFit));
    assert_eq!(started_on(&fx.provider.start(wanting(&[]))), Some("a"));
}

/// Asserts that `disk-gib` is a minimum: a job asking for the pool's promise exactly, or
/// less, is placed; one asking for more than the pool promises is refused as unservable;
/// and a host that reports less disk than the pool promises is not admitted.
#[test]
fn a_job_requiring_disk_is_placed_by_the_promise_hosts_were_held_to() {
    let mut fx = disk_pool("100");
    let small = reporting("s", &[("disk-gib", "99")]);
    assert_eq!(
        fx.hello(&small, &cred("t")).unwrap_err(),
        Refusal::ShapeNotAccepted
    );
    let bare = reporting("b", &[]);
    assert_eq!(
        fx.hello(&bare, &cred("t")).unwrap_err(),
        Refusal::ShapeNotAccepted
    );
    let big = reporting("big", &[("disk-gib", "250")]);
    fx.hello(&big, &cred("t")).unwrap();
    let more = fx.provider.start(wanting(&[("disk-gib", "101")]));
    assert_eq!(more, StartAck::Refused(RefusalCode::NoFit));
    let edge = fx.provider.start(wanting(&[("disk-gib", "100")]));
    assert_eq!(started_on(&edge), Some("big"));
}
