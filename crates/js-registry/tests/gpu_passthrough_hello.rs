//! The `gpu-passthrough` capability at admission, across releases. The key is information for
//! the operator: no pool, contract or placement reads it. The code that takes a hello here
//! (the hello's checks in `js-agent-wire`, admission in `js-registry`, the host contract in
//! `js-caps`) is unchanged since release v0.1.133 apart from the key's own constant, so these
//! are also the previous release's server taking this release's hello; a hello without the
//! key is what the previous release's agent sends.

mod support;

use js_registry::{PoolConfig, Refusal};
use js_types::{ExecMode, Resources, Shape};
use support::*;

/// A pool with one GPU per host that declares the container mode.
fn gpu_pool() -> Fixture {
    let resources = Resources::new(4000, 8 << 30, 1, Some("nvidia-a10".into())).unwrap();
    let shape = Shape::new(resources, map(&[("os", "macos")]));
    let name = js_types::ProviderId::new("builders").unwrap();
    let gpu = PoolConfig::new(name, class(), shape, 4);
    fixture_of(gpu.with_exec(ExecMode::Container))
}

/// A GPU host's hello reporting `pairs` besides `os`.
fn gpu_hello(host: &str, pairs: &[(&str, &str)]) -> js_agent_wire::Hello {
    let mut hello = hello(host);
    hello.capabilities.extend(map(pairs));
    hello.shape.gpus = 1;
    hello.shape.gpu_class = Some("nvidia-a10".into());
    hello
}

/// Asserts that a hello with `gpu-passthrough=true` passes the hello's checks and is admitted
/// as the same hello without it is (an agent of this release, and one of the previous
/// release, which never sends the key), in a pool that declares no mode and in a GPU pool
/// that declares the container mode; and that the key stands in for nothing the contract
/// needs: a GPU host that reports it and no `gpu-toolkit` is still refused naming
/// `gpu-toolkit`. Catches a server that refuses the new key, one that needs it of an older
/// agent, and a contract that takes it for the toolkit.
#[test]
fn gpu_passthrough_is_admitted_as_information_and_needed_by_no_pool() {
    let plain = fixture(4);
    let mut with = hello("with");
    with.capabilities
        .insert("gpu-passthrough".to_owned(), "true".to_owned());
    assert_eq!(with.validate(), Ok(()));
    assert!(plain.hello(&with, &cred("t")).is_ok());
    assert!(plain.hello(&hello("without"), &cred("t")).is_ok());
    assert_eq!(plain.registry.hosts().len(), 2);

    let gpus = gpu_pool();
    let toolkit = [("runtime", "container"), ("gpu-toolkit", "true")];
    let passing = [
        ("runtime", "container"),
        ("gpu-toolkit", "true"),
        ("gpu-passthrough", "true"),
    ];
    assert!(gpus.hello(&gpu_hello("old", &toolkit), &cred("t")).is_ok());
    assert!(gpus.hello(&gpu_hello("new", &passing), &cred("t")).is_ok());
    let no_toolkit = [("runtime", "container"), ("gpu-passthrough", "true")];
    let refused = gpus
        .registry
        .hello(&gpu_hello("crun", &no_toolkit), &cred("t"))
        .unwrap_err();
    assert_eq!(
        refused,
        Refusal::HostContract {
            field: "gpu-toolkit"
        }
    );
    assert_eq!(gpus.registry.hosts().len(), 2);
}
