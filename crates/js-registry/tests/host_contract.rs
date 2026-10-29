//! The host contract at admission: a pool that declares an execution mode admits only a
//! host that reports a runtime of that mode (and a toolkit, in a pool with GPUs), and a
//! pool that declares none admits hosts as before.

mod support;

use js_agent_wire::refusal;
use js_caps::Breach;
use js_registry::{PoolConfig, Refusal};
use js_types::{ExecMode, Resources, Shape};
use support::*;

/// A pool of macOS hosts that declares `exec`.
fn pool(exec: ExecMode) -> Fixture {
    fixture_of(config(4).with_exec(exec))
}

/// A pool with one GPU per host that declares the container mode.
fn gpu_pool() -> Fixture {
    let resources = Resources::new(4000, 8 << 30, 1, Some("nvidia-a10".into())).unwrap();
    let shape = Shape::new(resources, map(&[("os", "macos")]));
    let name = js_types::ProviderId::new("builders").unwrap();
    let gpu = PoolConfig::new(name, class(), shape, 4);
    fixture_of(gpu.with_exec(ExecMode::Container))
}

/// A hello that reports `pairs` besides `os`.
fn reporting(host: &str, pairs: &[(&str, &str)]) -> js_agent_wire::Hello {
    let mut hello = hello(host);
    hello.capabilities.extend(map(pairs));
    hello
}

/// A GPU host that reports `pairs` besides `os`.
fn gpu_reporting(host: &str, pairs: &[(&str, &str)]) -> js_agent_wire::Hello {
    let mut hello = reporting(host, pairs);
    hello.shape.gpus = 1;
    hello.shape.gpu_class = Some("nvidia-a10".into());
    hello
}

/// Asserts that a pool declaring `container` admits a host reporting the runtime and the
/// mode, and one reporting only the runtime. It catches a contract that refuses a host
/// that meets it.
#[test]
fn a_host_that_meets_the_contract_is_admitted() {
    let fx = pool(ExecMode::Container);
    let both = reporting("both", &[("runtime", "container"), ("exec", "container")]);
    assert!(fx.hello(&both, &cred("t")).is_ok());
    let only_runtime = reporting("only-runtime", &[("runtime", "container")]);
    assert!(fx.hello(&only_runtime, &cred("t")).is_ok());
    assert_eq!(fx.registry.hosts().len(), 2);
}

/// Asserts that a pool declaring a mode refuses a host that reports no runtime with
/// `host-contract` naming `runtime`, and creates nothing; an agent that predates the
/// capabilities reports none, so this is how it is refused. It catches a registry that
/// trusts the pool's word and admits a host that has nothing to run jobs with.
#[test]
fn no_runtime_is_refused_host_contract() {
    let fx = pool(ExecMode::Container);
    let refused = fx.registry.hello(&hello("old"), &cred("t")).unwrap_err();
    assert_eq!(refused, Refusal::HostContract { field: "runtime" });
    assert_eq!(refused.code(), refusal::HOST_CONTRACT);
    assert_eq!(refused.field(), Some("runtime"));
    assert!(refused.to_string().contains("runtime"), "{refused}");
    assert!(fx.registry.hosts().is_empty());
}

/// Asserts that a `sandbox-exec` pool refuses a hello from a container host with
/// `execution-mismatch` naming `exec`, that a host whose `exec` is absent but whose
/// `runtime` is the other mode is refused naming `runtime`, and that nothing is created.
/// It catches a pool that takes a host running jobs in the wrong mode.
#[test]
fn another_mode_is_refused_execution_mismatch() {
    let fx = pool(ExecMode::SandboxExec);
    let container = reporting("c", &[("runtime", "container"), ("exec", "container")]);
    let refused = fx.registry.hello(&container, &cred("t")).unwrap_err();
    assert_eq!(refused, Refusal::ExecutionMismatch { field: "exec" });
    assert_eq!(refused.code(), refusal::EXECUTION_MISMATCH);
    assert_eq!(refused.field(), Some("exec"));
    assert!(refused.to_string().contains("exec"), "{refused}");

    let runtime_only = reporting("r", &[("runtime", "container")]);
    let refused = fx.registry.hello(&runtime_only, &cred("t")).unwrap_err();
    assert_eq!(refused, Refusal::ExecutionMismatch { field: "runtime" });
    assert!(fx.registry.hosts().is_empty());

    let mac = reporting(
        "m",
        &[("runtime", "sandbox-exec"), ("exec", "sandbox-exec")],
    );
    assert!(fx.hello(&mac, &cred("t")).is_ok());
}

/// Asserts that a pool with GPUs needs the toolkit: a GPU host without it is refused
/// `host-contract` naming `gpu-toolkit`, and with it is admitted. It catches a GPU pool
/// that takes a host whose containers cannot reach the GPU.
#[test]
fn a_gpu_pool_needs_the_toolkit() {
    let fx = gpu_pool();
    let runtime = [("runtime", "container"), ("exec", "container")];
    let bare = gpu_reporting("bare", &runtime);
    let refused = fx.registry.hello(&bare, &cred("t")).unwrap_err();
    assert_eq!(
        refused,
        Refusal::HostContract {
            field: "gpu-toolkit"
        }
    );
    assert!(fx.registry.hosts().is_empty());

    let toolkit = [("runtime", "container"), ("gpu-toolkit", "true")];
    let whole = gpu_reporting("whole", &toolkit);
    assert!(fx.hello(&whole, &cred("t")).is_ok());
}

/// Asserts that the contract is checked before the shape: a host that is too small and
/// has no runtime is told about the runtime, the thing an operator fixes first. It
/// catches the two refusals swapped.
#[test]
fn the_contract_is_checked_before_the_shape() {
    let fx = pool(ExecMode::Container);
    let mut small = hello("small");
    small.shape.cpu_millis = 100;
    let refused = fx.registry.hello(&small, &cred("t")).unwrap_err();
    assert_eq!(refused, Refusal::HostContract { field: "runtime" });
    let mut small = reporting("small", &[("runtime", "container")]);
    small.shape.cpu_millis = 100;
    let refused = fx.registry.hello(&small, &cred("t")).unwrap_err();
    assert_eq!(refused, Refusal::ShapeNotAccepted);
}

/// Asserts the compatibility of a pool that declares no mode: it admits an agent that
/// reports none of the capabilities (an agent from before the contract) and one that
/// reports all of them, whatever their values. It catches a default that would refuse
/// every deployed agent when the server is upgraded.
#[test]
fn a_pool_without_a_mode_requires_nothing() {
    let fx = fixture(4);
    assert!(fx.hello(&hello("old"), &cred("t")).is_ok());
    let new = reporting(
        "new",
        &[
            ("runtime", "container"),
            ("exec", "container"),
            ("gpu-toolkit", "true"),
        ],
    );
    assert!(fx.hello(&new, &cred("t")).is_ok());
    let odd = reporting("odd", &[("runtime", "sandbox-exec")]);
    assert!(fx.hello(&odd, &cred("t")).is_ok());
}

/// Asserts that a breach converts to the refusal of its kind with the same field, and
/// that the other refusals name no field.
#[test]
fn breaches_become_refusals() {
    let missing = Refusal::from(Breach::Missing { field: "runtime" });
    assert_eq!(missing, Refusal::HostContract { field: "runtime" });
    let mismatch = Refusal::from(Breach::Mismatch { field: "exec" });
    assert_eq!(mismatch, Refusal::ExecutionMismatch { field: "exec" });
    assert_eq!(Refusal::ShapeNotAccepted.field(), None);
    assert_eq!(config(1).exec(), None);
    assert_eq!(
        config(1).with_exec(ExecMode::SandboxExec).exec(),
        Some(ExecMode::SandboxExec)
    );
}

/// Whether `host` reads as heard since its hello.
fn heard(fx: &Fixture, host_name: &str) -> bool {
    let hosts = fx.registry.hosts();
    hosts
        .iter()
        .find(|h| h.id == host(host_name))
        .unwrap()
        .heard_since_hello
}

/// Asserts the listing's `heard_since_hello`: false for a host that only said hello, true
/// once it heartbeats, and false again after a re-hello until its next heartbeat. It
/// catches a count that survives a new session, and one that never starts.
#[test]
fn a_host_is_heard_since_hello_only_after_a_heartbeat() {
    let fx = fixture(2);
    let epoch = fx.join("a", "t");
    assert!(!heard(&fx, "a"));
    fx.beat("a", epoch);
    assert!(heard(&fx, "a"));
    let epoch = fx.join("a", "t");
    assert!(!heard(&fx, "a"));
    fx.beat("a", epoch);
    assert!(heard(&fx, "a"));
}
