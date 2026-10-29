//! The host contract between a real agent and a real lease server, both ways: an agent
//! that reports its runtime joins a pool that declares a mode (and one that declares
//! none), and a pool that the host does not meet refuses it with the capability named, so
//! the agent ends saying what to fix. The container runtime is in memory.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::drain::container_flags;
use crate::server::{Agent, Redirect, Server};
use crate::support::{Host, Ran, Scratch, code, linux_host};

/// A pool of Linux hosts whose mode is `container`.
const CONTAINER_POOL: &str = r#"
exec = "container"
name = "macs"
class = "mac"
max_machines = 4

[shape]
cpu_millis = 4000
memory_bytes = 4294967296

[capabilities]
os = "linux"
"#;

/// The same pool, declaring `sandbox-exec`.
const SANDBOX_POOL: &str = r#"
exec = "sandbox-exec"
name = "macs"
class = "mac"
max_machines = 4

[shape]
cpu_millis = 4000
memory_bytes = 4294967296

[capabilities]
os = "linux"
"#;

/// A pool of container hosts with one A100 each.
const GPU_POOL: &str = r#"
exec = "container"
name = "macs"
class = "mac"
max_machines = 4

[shape]
cpu_millis = 4000
memory_bytes = 4294967296
gpus = 1
gpu_class = "nvidia-a100"

[capabilities]
os = "linux"
"#;

/// The toolkit check of [`toolkit_host`].
const HOOK: &str = "/opt/js/gpu-toolkit-check";

/// A Linux host whose toolkit check passes.
fn toolkit_host() -> Host {
    let mut host = linux_host();
    host.commands = host.commands.with(&format!("{HOOK} "), Ok(""));
    host
}

/// Starts a server over `pool` and an agent over containers with `extra` flags on `host`;
/// returns both, and the agent's files.
fn start(
    name: &str,
    pool: &'static str,
    extra: &[&str],
    host: fn() -> Host,
) -> (Server, Agent, Scratch) {
    let server = Server::start_pool(name, &[], pool);
    let files = Scratch::new(&format!("{name}-files"));
    let mut owned = container_flags(&files);
    owned.extend(["--host-id".to_owned(), "h1".to_owned()]);
    owned.extend(extra.iter().map(|flag| (*flag).to_owned()));
    let all: Vec<&str> = owned.iter().map(String::as_str).collect();
    let address = Arc::new(Mutex::new(server.url()));
    let agent = Agent::spawn(
        server.url(),
        server.join_file(),
        &all,
        host,
        Redirect(address),
    );
    (server, agent, files)
}

/// Asserts that the agent is admitted: it says hello and the server answers with a welcome
/// (the agent then waits its heartbeat interval). A pool that declares a mode admits only a
/// host that reported what the mode needs, so this is also the proof that it did.
fn admitted_with(name: &str, pool: &'static str, extra: &[&str], host: fn() -> Host) {
    let (_server, agent, _files) = start(name, pool, extra, host);
    assert_eq!(agent.next_wait(), Duration::from_secs(5), "the hello");
    let ran = agent.stop();
    assert_eq!(ran.status(), code(0), "{}", ran.err);
}

/// The end of an agent that the server refuses.
fn refused_with(name: &str, pool: &'static str, extra: &[&str], host: fn() -> Host) -> Ran {
    let (_server, agent, _files) = start(name, pool, extra, host);
    agent.finish()
}

/// Asserts that a pool that declares `container` admits an agent that runs containers, and
/// that a pool that declares nothing does too. The server's listing shows the pool's shape
/// and not the host's claim, so admission is the evidence. It catches an agent whose hello
/// the registry does not accept, in either configuration.
#[test]
fn a_container_agent_joins_a_pool_with_and_without_a_mode() {
    admitted_with("contract_e2e_container", CONTAINER_POOL, &[], linux_host);
    admitted_with(
        "contract_e2e_none",
        crate::server::POOL_RUNS_TOML,
        &[],
        linux_host,
    );
}

/// Asserts that a pool that declares `sandbox-exec` refuses a container agent: the agent
/// ends with status 2 and prints `execution-mismatch` with the capability `exec`. It
/// catches a mismatch the operator is not told about, and a refusal the agent retries.
#[test]
fn another_mode_ends_the_agent_naming_the_capability() {
    let ran = refused_with("contract_e2e_sandbox", SANDBOX_POOL, &[], linux_host);
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    let said = "status 403 (execution-mismatch; the capability at fault is exec)";
    assert!(ran.err.contains(said), "{}", ran.err);
}

/// Asserts that a pool with GPUs refuses a GPU host that has not shown its toolkit
/// (`host-contract`, capability `gpu-toolkit`), and admits the same host when its
/// `--gpu-toolkit-hook` exits 0. It catches a GPU pool that takes a host whose containers
/// cannot use the GPU, and a hook that is run but not reported.
#[test]
fn a_gpu_pool_needs_the_toolkit_hook() {
    let class = ["--gpus", "1", "--gpu-class", "nvidia-a100"];
    let ran = refused_with("contract_e2e_notoolkit", GPU_POOL, &class, linux_host);
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    let said = "status 403 (host-contract; the capability at fault is gpu-toolkit)";
    assert!(ran.err.contains(said), "{}", ran.err);

    let hooked = [
        "--gpus",
        "1",
        "--gpu-class",
        "nvidia-a100",
        "--gpu-toolkit-hook",
        HOOK,
    ];
    admitted_with("contract_e2e_toolkit", GPU_POOL, &hooked, toolkit_host);
}
