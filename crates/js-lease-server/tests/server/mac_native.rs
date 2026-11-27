//! How a job is run follows from its operating system, against a real server on loopback with
//! two pools: `burst`, Linux hosts on x86_64 in containers, and `mac-build`, Macs on arm64 run
//! natively under `sandbox-exec`. Jobs are submitted through the job service's gateway, placed
//! by the scheduler's pass, and their starts read from the heartbeat reply of the host they
//! were placed on.
//!
//! A macOS job that names a bundle and no execution mode is placed on a Mac and started
//! natively, as one that names `sandbox-exec` is; with no Mac registered it is refused
//! `no-capable-host` at submit; a Linux job never lands on a Mac, and its container start
//! carries the job's system and architecture.

use serde_json::{Value, json};

use crate::agents_support::{Session, job_hello, post, session};
use crate::jobs_support::{JobBody, job_agent, job_args, job_of, submit_body};
use crate::support::{Harness, Scratch};

/// The Linux hosts' pool: up to two machines of 8 cores and 16 GiB, Linux on x86_64.
const LINUX_POOL: &str = r#"
name = "burst"
class = "burst-host"
max_machines = 2

[shape]
cpu_millis = 8000
memory_bytes = 17179869184

[capabilities]
os = "linux"
arch = "x86_64"
"#;

/// The Macs' pool: up to two Macs of 24 cores and 64 GiB, macOS on
/// arm64, run natively under `sandbox-exec`.
const MAC_POOL: &str = r#"
name = "mac-build"
class = "mac-host"
max_machines = 2
exec = "sandbox-exec"

[shape]
cpu_millis = 24000
memory_bytes = 68719476736

[capabilities]
os = "macos"
arch = "arm64"
"#;

/// The Linux host and the Mac the tests register.
const LINUX: &str = "node-1";
const MAC: &str = "mac-1";

/// The hello of the Mac `host` in `mac-build`: macOS on arm64 in the native runtime, speaking
/// the per-job messages.
fn mac_hello(host: &str) -> Value {
    json!({
        "host_id": host,
        "pool": "mac-build",
        "agent_version": "0.1.0",
        "agent_process": format!("process-of-{host}"),
        "shape": { "cpu_millis": 24000, "memory_bytes": 68719476736u64, "gpus": 0 },
        "capabilities": {
            "os": "macos",
            "arch": "arm64",
            "exec": "sandbox-exec",
            "runtime": "sandbox-exec",
            "per-job": "true",
        },
        "phase": "serving",
    })
}

/// A server of [`LINUX_POOL`] and [`MAC_POOL`] with the job service on, and nothing
/// registered.
async fn two_pools(name: &str) -> Harness {
    let scratch = Scratch::new(name);
    let mut args = job_args(&scratch, "127.0.0.1:0");
    scratch.write("agent-pool.toml", LINUX_POOL, 0o644);
    let mac = scratch.write("mac-pool.toml", MAC_POOL, 0o644);
    args.extend(["--agent-pool-config".into(), mac.into_os_string()]);
    Harness::with(scratch, args).await
}

/// The Linux host's per-job agent, registered in `burst`.
async fn linux_agent(h: &Harness) -> Agent {
    let mut hello = job_hello(LINUX);
    hello["capabilities"]["os"] = json!("linux");
    Agent(job_agent(h, &hello).await)
}

/// The Mac's per-job agent, registered in `mac-build`.
async fn mac_agent(h: &Harness) -> Agent {
    let reply = post(h, "/v1/agent/hello", None, &mac_hello(MAC)).await;
    Agent(session(&reply, MAC))
}

/// A registered agent: its session.
struct Agent(Session);

/// The body of the job `id` of the bundle `team/kbf-task` with no command and the arguments of
/// a build task, 1 core and 1 GiB, with `constraints`.
fn bundle_job(id: &str, constraints: Value) -> Value {
    let mut body = JobBody::new(id).constraints(constraints).json();
    let spec = &mut body["spec"];
    spec["image"] = json!(format!("team/kbf-task@sha256:{}", "b".repeat(64)));
    spec.as_object_mut().unwrap().remove("command");
    spec["args"] = json!(["--task", "build"]);
    body
}

/// The constraints of a job on macOS and arm64 that names no execution mode.
fn macos() -> Value {
    json!({ "os": "macos", "arch": "arm64" })
}

/// Submits `body`, which must be accepted, steps a tick, and returns the host the job `id` was
/// placed on.
async fn placed(h: &Harness, id: &str, body: &Value) -> String {
    let reply = submit_body(h, body).await;
    assert_eq!(reply.status, 201, "{id}: {}", reply.body);
    h.ticks.step().await;
    let read = job_of(h, id).await.json();
    let host = read["job"]["hostId"].as_str();
    host.unwrap_or_else(|| panic!("{id} was not placed: {read}"))
        .to_owned()
}

/// The job starts the heartbeat reply of `agent` carries, as the wire has them.
async fn starts(h: &Harness, agent: &Agent) -> Vec<Value> {
    let Agent(session) = agent;
    let reply = post(h, "/v1/agent/heartbeat", None, &session.beat()).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    let commands = reply.json()["commands"].as_array().cloned();
    let starts = commands.into_iter().flatten();
    starts
        .filter(|command| command["type"] == "job_start")
        .collect()
}

/// The one start `agent`'s next heartbeat reply carries.
async fn one_start(h: &Harness, agent: &Agent) -> Value {
    let mut starts = starts(h, agent).await;
    assert_eq!(starts.len(), 1, "{starts:?}");
    starts.remove(0)
}

/// Asserts that a native start, as the wire carries it, runs the bundle's default entrypoint
/// with the job's arguments, has no command, and names macOS on arm64.
fn assert_native(start: &Value, id: &str) {
    assert_eq!(start["job_id"], id, "{start}");
    assert_eq!(start["run"]["program"], "@bundle/kbf-task", "{start}");
    assert_eq!(start["run"]["args"], json!(["--task", "build"]), "{start}");
    assert_eq!(start.get("command"), None, "{start}");
    assert_eq!(
        (&start["os"], &start["arch"]),
        (&json!("macos"), &json!("arm64"))
    );
}

/// Asserts that a job on macOS and arm64 with a bundle and no execution mode, submitted while a
/// Linux host and a Mac that reports `exec=sandbox-exec` are registered, is placed on the Mac
/// and its start is native: a `run` of the bundle's entrypoint, no command, and `os` and `arch`
/// set. Catches a macOS bundle placed on the Mac but sent as a container command (how it was
/// started while only `sandbox-exec` made a job native), and one placed on the Linux host.
#[tokio::test]
async fn a_macos_bundle_with_no_exec_mode_is_placed_on_a_mac_and_started_natively() {
    let h = two_pools("mac_native_no_exec").await;
    let linux = linux_agent(&h).await;
    let mac = mac_agent(&h).await;
    let host = placed(&h, "mac-job", &bundle_job("mac-job", macos())).await;
    assert_eq!(host, MAC);
    assert_native(&one_start(&h, &mac).await, "mac-job");
    assert!(starts(&h, &linux).await.is_empty());
}

/// Asserts that a client that still sends `exec=sandbox-exec` beside macOS and arm64 gets what
/// it got before: the job is placed on the Mac and started natively, with the same start as a
/// job that names no mode. Catches an older client's native job that the rule by operating
/// system turns into a container start.
#[tokio::test]
async fn a_macos_bundle_naming_sandbox_exec_is_started_natively_as_before() {
    let h = two_pools("mac_native_sandbox_exec").await;
    linux_agent(&h).await;
    let mac = mac_agent(&h).await;
    let mut constraints = macos();
    constraints["exec"] = json!("EXEC_MODE_SANDBOX_EXEC");
    let host = placed(&h, "old-client", &bundle_job("old-client", constraints)).await;
    assert_eq!(host, MAC);
    assert_native(&one_start(&h, &mac).await, "old-client");
}

/// Asserts that with no Mac registered (the Macs' pool configured, a Linux host registered) a
/// job on macOS and arm64 with a bundle and no execution mode is refused at submit with
/// `no-capable-host` and is not recorded, as a job that names `sandbox-exec` is; and that a
/// macOS job that names `container` is refused the same way while a Mac is registered, since
/// no host runs macOS in containers. Catches a macOS job queued for good, or placed on the
/// Linux host, when no Mac can take it.
#[tokio::test]
async fn a_macos_job_with_no_mac_to_run_it_is_refused_no_capable_host() {
    let h = two_pools("mac_native_no_mac").await;
    linux_agent(&h).await;
    let mut named = macos();
    named["exec"] = json!("EXEC_MODE_SANDBOX_EXEC");
    for (id, constraints) in [("no-exec", macos()), ("sandbox-exec", named)] {
        let refused = submit_body(&h, &bundle_job(id, constraints)).await;
        assert_eq!(refused.status, 422, "{id}: {}", refused.body);
        assert_eq!(
            refused.json(),
            json!({ "error": "no-capable-host" }),
            "{id}"
        );
        assert_eq!(job_of(&h, id).await.status, 404, "{id}");
    }
    mac_agent(&h).await;
    let mut contained = macos();
    contained["exec"] = json!("EXEC_MODE_CONTAINER");
    let refused = submit_body(&h, &bundle_job("contained", contained)).await;
    assert_eq!(refused.status, 422, "{}", refused.body);
    assert_eq!(refused.json(), json!({ "error": "no-capable-host" }));
}

/// Asserts that a Linux job never lands on a Mac: with only the Mac registered it is refused
/// `no-capable-host`; with the Linux host registered too, two Linux jobs are each placed on
/// the Linux host though the Mac is idle, the Mac's heartbeat carries no start, and each start
/// is a container start (its command, no `run`) that carries `os=linux` and `arch=x86_64`.
/// Catches a Linux job placed on a Mac, and a container start that drops the job's system and
/// architecture.
#[tokio::test]
async fn a_linux_job_never_lands_on_a_mac_and_its_start_names_its_platform() {
    let h = two_pools("mac_native_linux").await;
    let mac = mac_agent(&h).await;
    let linux_job = |id: &str| {
        let constraints = json!({ "os": "linux", "arch": "x86_64" });
        JobBody::new(id).constraints(constraints).json()
    };
    let refused = submit_body(&h, &linux_job("only-mac")).await;
    assert_eq!(refused.status, 422, "{}", refused.body);
    assert_eq!(refused.json(), json!({ "error": "no-capable-host" }));
    let linux = linux_agent(&h).await;
    for id in ["linux-a", "linux-b"] {
        assert_eq!(placed(&h, id, &linux_job(id)).await, LINUX);
    }
    assert!(starts(&h, &mac).await.is_empty());
    let started = starts(&h, &linux).await;
    let ids: Vec<&Value> = started.iter().map(|start| &start["job_id"]).collect();
    assert_eq!(ids, [&json!("linux-a"), &json!("linux-b")]);
    for start in &started {
        assert_eq!(start["command"], json!(["run"]), "{start}");
        assert_eq!(start.get("run"), None, "{start}");
        assert_eq!(
            (&start["os"], &start["arch"]),
            (&json!("linux"), &json!("x86_64"))
        );
    }
}
