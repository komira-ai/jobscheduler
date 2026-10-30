//! Native jobs and the capabilities a host reports for them: a native start goes only to a
//! host whose hello said `exec` is `sandbox-exec`, and a start that pins a bundle only to
//! one that also reported a bundle store. Whatever the host reports, an image job is
//! queued as it always was.

mod support;

use js_agent_wire::{Command, Hello, Isolation, JobSpec, Limits, NativeSpec, NetworkLevel};
use js_provider::Provider;
use support::*;

const BUNDLE: &str =
    "toolchain:1@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn limits() -> Limits {
    Limits {
        cpu_millis: 1000,
        memory_bytes: 1 << 20,
        disk_bytes: None,
        pids: None,
        wall_ms: 60_000,
    }
}

fn job(run: &str, image: &str, native: Option<NativeSpec>, bundle: Option<&str>) -> JobSpec {
    JobSpec {
        run_id: run.into(),
        lease_id: String::new(),
        image: image.into(),
        native,
        bundle: bundle.map(str::to_owned),
        args: Vec::new(),
        limits: limits(),
        gpu_indices: Vec::new(),
        mounts: Vec::new(),
        files: Vec::new(),
        isolation: Isolation::Contained,
        labels: Default::default(),
        heartbeat_seq: 0,
        valid_for_ms: 0,
    }
}

fn native(program: &str) -> NativeSpec {
    NativeSpec {
        program: program.into(),
        args: vec!["--version".into()],
        env: Default::default(),
        cwd: String::new(),
        inputs: Vec::new(),
        outputs: Vec::new(),
        network: NetworkLevel::Loopback,
    }
}

fn image_job(run: &str) -> JobSpec {
    let image = "busybox:1@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    job(run, image, None, None)
}

fn hello_with(capabilities: &[(&str, &str)]) -> Hello {
    let mut hello = hello("a");
    for (key, value) in capabilities {
        hello.capabilities.insert((*key).into(), (*value).into());
    }
    hello
}

/// A fixture with host `a` leased by lease 1, its hello reporting `capabilities`. Returns
/// the fixture and the epoch.
fn leased(capabilities: &[(&str, &str)]) -> (Fixture, u64) {
    let fx = fixture(4);
    let epoch = fx
        .hello(&hello_with(capabilities), &cred("t"))
        .unwrap()
        .epoch;
    assert!(started_on(&fx.provider.clone().start(spec(1))).is_some());
    (fx, epoch)
}

fn queue(fx: &Fixture, spec: JobSpec) -> Result<(), String> {
    let queued = fx.registry.queue_run(&host("a"), &lease(1), spec);
    queued
        .map(drop)
        .map_err(|refusal| refusal.code().to_owned())
}

/// Asserts that a native start is queued for a host reporting `exec` as `sandbox-exec`,
/// and goes out on the next heartbeat with its native body.
#[test]
fn a_native_start_goes_to_a_native_host() {
    let (fx, epoch) = leased(&[("exec", "sandbox-exec")]);
    assert_eq!(
        queue(&fx, job("r1", "", Some(native("/usr/bin/true")), None)),
        Ok(())
    );
    let message = fx.heartbeat("a", epoch);
    let sent = fx
        .registry
        .heartbeat(&message, &cred("t"))
        .unwrap()
        .commands;
    let [Command::Start(start)] = &sent[..] else {
        panic!("{sent:?}");
    };
    assert_eq!(start.native, Some(native("/usr/bin/true")));
}

/// Asserts that a native start is refused `native-unsupported` for a host that reported
/// another mode, a mode spelled with other letters, or none, with nothing queued; and that an
/// image job is queued on each of those hosts. It catches a native job placed where its
/// agent would refuse it, or read it as a container job.
#[test]
fn a_native_start_is_refused_for_any_other_host() {
    for exec in [None, Some("container"), Some("Sandbox-Exec")] {
        let capabilities: Vec<(&str, &str)> = exec.map(|mode| ("exec", mode)).into_iter().collect();
        let (fx, _) = leased(&capabilities);
        let refused = queue(&fx, job("r1", "", Some(native("/usr/bin/true")), None));
        assert_eq!(refused, Err("native-unsupported".to_owned()), "{exec:?}");
        assert!(fx.registry.runs_of(&lease(1)).is_empty(), "{exec:?}");
        assert_eq!(queue(&fx, image_job("r2")), Ok(()), "{exec:?}");
    }
}

/// Asserts that a start pinning a bundle is queued for a native host that reported a
/// store (`bundle-root`), and refused `bundle-unsupported` for one that did not, with
/// nothing queued.
#[test]
fn a_bundle_start_needs_a_host_with_a_store() {
    let with_bundle = job("r1", "", Some(native("@bundle/clang")), Some(BUNDLE));
    let (fx, _) = leased(&[("exec", "sandbox-exec"), ("bundle-root", "/opt/bundles")]);
    assert_eq!(queue(&fx, with_bundle.clone()), Ok(()));
    let (fx, _) = leased(&[("exec", "sandbox-exec")]);
    assert_eq!(
        queue(&fx, with_bundle),
        Err("bundle-unsupported".to_owned())
    );
    assert!(fx.registry.runs_of(&lease(1)).is_empty());
}

/// Asserts that a native job with no bundle is queued for a native host that has no store.
#[test]
fn a_native_start_without_a_bundle_needs_no_store() {
    let (fx, _) = leased(&[("exec", "sandbox-exec")]);
    assert_eq!(
        queue(&fx, job("r1", "", Some(native("/usr/bin/true")), None)),
        Ok(())
    );
}

/// Asserts that the wire's own rules still come first: a start with an image and a native
/// body is `invalid-image`, even for a host that cannot run native jobs.
#[test]
fn the_wire_rules_are_checked_before_the_host() {
    let (fx, _) = leased(&[]);
    let both = job("r1", "busybox:1", Some(native("/usr/bin/true")), None);
    assert_eq!(queue(&fx, both), Err("invalid-image".to_owned()));
}
