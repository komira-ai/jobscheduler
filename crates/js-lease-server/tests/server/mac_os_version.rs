//! The Macs' pool and the least macOS version of a job (`constraints.osVersion`), against a
//! real server on loopback: one pool `mac-build` whose `[capabilities]` name the floor
//! `os_version = "26"` (the policy of `docs/design/mac-runtime.md`, section 5), four Macs,
//! two of which report macOS 27 at their hello and two macOS 26, and jobs submitted through
//! the job service.
//!
//! A host is placed by the major version its hello reports, not by the pool's floor
//! (`placement_shape` in `crates/js-registry/src/config.rs`); the registry's own test of
//! that is `a_host_is_placed_by_the_os_version_it_reports_in_every_pool`. These tests hold
//! the whole server to it: submit, admission, the scheduler's pass and the hello's refusal.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::agents_support::post;
use crate::jobs_support::{JobBody, job_agent, job_args, job_of, submit_body};
use crate::support::{Harness, Scratch};

/// The Macs' pool as the server is given it: up to four Mac Studios booked at the smallest
/// one's 28 cores and 96 GiB, macOS on arm64 run natively under `sandbox-exec`, and the
/// least macOS version every Mac of the farm runs, 26.
const MAC_POOL: &str = r#"
name = "mac-build"
class = "mac-host"
max_machines = 4
exec = "sandbox-exec"

[shape]
cpu_millis = 28000
memory_bytes = 103079215104

[capabilities]
os = "macos"
arch = "arm64"
os_version = "26"
"#;

/// The two Macs that report macOS 27; the first has 32 cores, more than the pool books.
const ON_27: [&str; 2] = ["mac-27-a", "mac-27-b"];

/// The two Macs that report macOS 26 (one runs 26.4 and one 26.6; the hello carries the
/// major version alone).
const ON_26: [&str; 2] = ["mac-26-a", "mac-26-b"];

/// The cores of one test job: half the pool's 28, so a Mac holds two and not three.
const HALF_A_MAC: u64 = 14_000;

/// The hello of the Mac `host` with 28 cores (32 for `mac-27-a`) and 96 GiB, in the pool
/// `mac-build`, speaking the per-job messages, reporting `os_version` when `version` gives
/// one.
fn mac_hello(host: &str, version: Option<&str>) -> Value {
    let cores: u64 = if host == "mac-27-a" { 32_000 } else { 28_000 };
    let mut hello = json!({
        "host_id": host,
        "pool": "mac-build",
        "agent_version": "0.1.0",
        "agent_process": format!("process-of-{host}"),
        "shape": { "cpu_millis": cores, "memory_bytes": 103079215104u64, "gpus": 0 },
        "capabilities": {
            "os": "macos",
            "arch": "arm64",
            "exec": "sandbox-exec",
            "runtime": "sandbox-exec",
            "per-job": "true",
        },
        "phase": "serving",
    });
    if let Some(version) = version {
        hello["capabilities"]["os_version"] = json!(version);
    }
    hello
}

/// A server over [`MAC_POOL`] with the job service on, and the four Macs registered.
async fn four_macs(name: &str) -> Harness {
    let scratch = Scratch::new(name);
    let args = job_args(&scratch, "127.0.0.1:0");
    scratch.write("agent-pool.toml", MAC_POOL, 0o644);
    let h = Harness::with(scratch, args).await;
    for (hosts, version) in [(ON_27, "27"), (ON_26, "26")] {
        for host in hosts {
            job_agent(&h, &mac_hello(host, Some(version))).await;
        }
    }
    h
}

/// The body of the job `id`: half a Mac's cores and 1 GiB, on macOS under `sandbox-exec`, of
/// at least the macOS major `version`.
fn mac_job(id: &str, version: &str) -> Value {
    let constraints = json!({
        "os": "macos",
        "exec": "EXEC_MODE_SANDBOX_EXEC",
        "osVersion": version,
    });
    JobBody::new(id)
        .needs(HALF_A_MAC, 1 << 30)
        .constraints(constraints)
        .json()
}

/// Submits the job `id` of at least macOS `version`, which must be accepted, steps a tick,
/// and returns the host it was placed on (`None` while it waits in the queue).
async fn place(h: &Harness, id: &str, version: &str) -> Option<String> {
    let reply = submit_body(h, &mac_job(id, version)).await;
    assert_eq!(reply.status, 201, "{id}: {}", reply.body);
    h.ticks.step().await;
    let read = job_of(h, id).await.json();
    let host = read["job"].get("hostId").and_then(Value::as_str);
    match host {
        Some(_) => assert_ne!(read["job"]["state"], "JOB_STATE_QUEUED", "{id}: {read}"),
        None => assert_eq!(read["job"]["state"], "JOB_STATE_QUEUED", "{id}: {read}"),
    }
    host.map(str::to_owned)
}

/// How many of `placed` went to each host.
fn per_host(placed: &[String]) -> BTreeMap<&str, usize> {
    let mut counts = BTreeMap::new();
    for host in placed {
        *counts.entry(host.as_str()).or_insert(0) += 1;
    }
    counts
}

/// Asserts that jobs asking for macOS 27 are placed only on the two Macs that report 27:
/// four jobs of half a Mac each fill those two Macs, two each, and a fifth waits in the
/// queue although both Macs on 26 are empty. Catches a Mac placed by the pool's floor (26)
/// in place of the version it runs, which would leave every job asking for 27 unplaced,
/// and a version compared in the wrong direction, which would put the fifth job on a Mac
/// on 26.
#[tokio::test]
async fn a_job_asking_for_macos_27_is_placed_only_on_the_macs_on_27() {
    let h = four_macs("mac_os_version_27").await;
    let mut placed = Vec::new();
    for index in 0..4 {
        let id = format!("needs-27-{index}");
        let host = place(&h, &id, "27").await;
        placed.push(host.unwrap_or_else(|| panic!("{id} was not placed")));
    }
    let want: BTreeMap<&str, usize> = ON_27.into_iter().map(|host| (host, 2)).collect();
    assert_eq!(per_host(&placed), want);
    assert_eq!(place(&h, "needs-27-4", "27").await, None);
}

/// Asserts that jobs asking for macOS 26 are placed on all four Macs, those on 26 and those
/// on 27: eight jobs of half a Mac each fill the four, two each, and a ninth waits in the
/// queue. Catches a version compared as equal rather than as at least, which would keep a
/// job asking for 26 off the Macs on 27, and a floor that refused the Macs on 26.
#[tokio::test]
async fn a_job_asking_for_macos_26_is_placed_on_all_four_macs() {
    let h = four_macs("mac_os_version_26").await;
    let mut placed = Vec::new();
    for index in 0..8 {
        let id = format!("needs-26-{index}");
        let host = place(&h, &id, "26").await;
        placed.push(host.unwrap_or_else(|| panic!("{id} was not placed")));
    }
    let want: BTreeMap<&str, usize> = ON_27
        .into_iter()
        .chain(ON_26)
        .map(|host| (host, 2))
        .collect();
    assert_eq!(per_host(&placed), want);
    assert_eq!(place(&h, "needs-26-8", "26").await, None);
}

/// Asserts that a job asking for a macOS later than any Mac reports (28) is refused at
/// submit with `no-capable-host` and is not recorded, while the four Macs are registered.
/// Catches a least version that is ignored, which would place the job on a Mac on 27.
#[tokio::test]
async fn a_job_asking_for_a_later_macos_than_any_mac_runs_is_refused() {
    let h = four_macs("mac_os_version_28").await;
    let refused = submit_body(&h, &mac_job("needs-28", "28")).await;
    assert_eq!(refused.status, 422, "{}", refused.body);
    assert_eq!(refused.json(), json!({ "error": "no-capable-host" }));
    assert_eq!(job_of(&h, "needs-28").await.status, 404);
}

/// Asserts what the pool's floor admits at the hello: a Mac that reports macOS 25, and one
/// that reports no version (its probe failed, or its agent predates the report), are each
/// refused `shape-not-accepted` with the reason `below-floor`, and a Mac that reports 26 or
/// 27 is admitted. Catches a floor that is not applied at the hello, which would register a
/// Mac the macOS runtime was never run on, or one that serves no job asking for a version.
#[tokio::test]
async fn the_floor_refuses_a_mac_below_26_or_reporting_no_version() {
    let scratch = Scratch::new("mac_os_version_floor");
    let args = job_args(&scratch, "127.0.0.1:0");
    scratch.write("agent-pool.toml", MAC_POOL, 0o644);
    let h = Harness::with(scratch, args).await;
    for (host, version) in [("mac-25", Some("25")), ("mac-unknown", None)] {
        let reply = post(&h, "/v1/agent/hello", None, &mac_hello(host, version)).await;
        assert_eq!(reply.status, 403, "{host}: {}", reply.body);
        assert_eq!(
            reply.json(),
            json!({ "error": "shape-not-accepted", "reason": "below-floor" }),
            "{host}"
        );
    }
    job_agent(&h, &mac_hello("mac-26-a", Some("26"))).await;
    job_agent(&h, &mac_hello("mac-27-a", Some("27"))).await;
}
