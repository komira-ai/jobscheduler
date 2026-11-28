//! The hosts listing over the registry's hosts and the `node/` rows of the job store.

use std::sync::Arc;
use std::time::Duration;

use js_agent_wire::Phase;
use js_jobs::{Isolation, ScaleMark};
use serde_json::{Value, json};

use super::*;
use crate::agent_api::job_rig::{BOOKED, Rig, clocked_rig, placed, rig, spec};
use crate::agent_api::peer_rig::NEVER;
use crate::agent_api::peer_rig::{Row, peer_of, write_row};
use crate::api::rig::{Lines, process_sink};
use crate::config::parse_token;
use crate::job_api::fleet::Hosts;

/// The shared state over `rig`'s server.
fn shared_over(rig: &Rig) -> Shared {
    Shared::new(parse_token(&[b'q'; 40]).unwrap(), Arc::clone(&rig.agents))
}

/// The listing of `rig`'s server as the body writes it, by host id.
async fn listed(rig: &Rig) -> Value {
    let list = listing_with_rows(&shared_over(rig)).await;
    serde_json::to_value(&list).unwrap()
}

fn entry<'a>(body: &'a Value, host: &str) -> &'a Value {
    body["hosts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|one| one["id"] == host)
        .unwrap_or_else(|| panic!("{host} is not listed in {body}"))
}

fn ids(body: &Value) -> Vec<&str> {
    let hosts = body["hosts"].as_array().unwrap();
    hosts
        .iter()
        .map(|one| one["id"].as_str().unwrap())
        .collect()
}

/// Asserts that the listing of a server lists a host only a row knows (it said hello to
/// another server) beside its own, each once and in id order, with the state from when it was
/// last heard, the phase, shape and capabilities, labels, the agent version its hello gave and
/// no registry-only fields; and that a host in both the registry and the rows (each host whose
/// hello it took) is listed as the registry sees it. Catches a listing that shows only the
/// hosts a server took the hello of, one that lists a host twice, and one that drops the agent
/// version or the orphans of a host only a row knows.
#[tokio::test]
async fn the_listing_shows_a_host_only_a_row_knows_once() {
    let mut first = clocked_rig("hosts_rows_a", false, NEVER);
    first.join("host-a", false).await;
    let mut second = peer_of("hosts_rows_b", &first);
    second.join("host-b", false).await;
    second.join("host-c", false).await;
    // host-a's row holds two orphans: a host only a row knows shows them as its row does.
    let host_a = js_types::HostId::new("host-a").unwrap();
    let row = first.jobs.read_node(&host_a).unwrap().unwrap();
    let orphan = js_jobs::Orphan {
        first_seen: row.record.heard_at,
        end_asked: false,
    };
    let state = js_jobs::NodeState {
        orphans: [("j1".to_owned(), orphan), ("j2".to_owned(), orphan)].into(),
        ..row.record.state.clone()
    };
    first.jobs.settle_node(&row, state).unwrap().unwrap();
    let body = listed(&second).await;
    assert_eq!(ids(&body), ["host-a", "host-b", "host-c"]);
    assert_eq!(entry(&body, "host-a")["orphans"], 2);
    assert!(entry(&body, "host-b").get("orphans").is_none());

    let shape = json!({"cpu_millis":8000,"memory_bytes":17179869184u64,"gpus":0});
    let by_row = entry(&body, "host-a");
    assert_eq!(by_row["source"], "registered");
    assert_eq!(by_row["state"], "live");
    assert_eq!(by_row["lost"], false);
    assert_eq!(by_row["phase"], "serving");
    assert_eq!(by_row["releasing"], false);
    assert_eq!(by_row["memory"]["bookable_bytes"], 17_179_869_184u64);
    assert!(by_row["capabilities"].get("arch").is_some());
    assert_eq!(by_row["agent_version"], "0.1.0");
    assert!(by_row.get("mark").is_none());
    assert!(by_row.get("taint").is_none());
    assert!(by_row["shape"].get("cpu_millis").is_some());
    assert_ne!(by_row["shape"], json!({}));
    assert_eq!(by_row["shape"]["memory_bytes"], shape["memory_bytes"]);
    assert_eq!(entry(&body, "host-c")["agent_version"], "0.1.0");
    assert_eq!(entry(&body, "host-b")["agent_version"], "0.1.0");
}

/// Asserts that a host known by its row alone is listed lost once it was last heard more than
/// the longest grace ago, shows the drain and release marks of its `hostmark` row (a release
/// also as `releasing`) and the taint of its `taint` row, and takes its capabilities from the
/// booked shape when its hello had no claim. Catches a stale row listed live, and a mark or
/// taint the listing drops.
#[tokio::test]
async fn a_host_only_a_row_knows_shows_its_age_marks_and_taint() {
    let mut first = clocked_rig("hosts_rows_marks", false, NEVER);
    let row = |host| Row {
        host,
        pool: "burst",
        host_jobs: None,
        booked: true,
    };
    write_row(&first.jobs, &row("host-n"), Phase::MemoryPressure);
    for name in ["host-d", "host-r", "host-t"] {
        first.join(name, false).await;
    }
    let mark = |name: &str, mark| {
        let host = HostId::new(name).unwrap();
        assert!(first.jobs.mark_host(&host, mark, None).unwrap());
    };
    mark("host-d", ScaleMark::Draining);
    mark("host-r", ScaleMark::Releasing);
    let mut host_job = spec();
    host_job.constraints.isolation = Isolation::Host;
    placed(&first.jobs, "job-t", "host-t", host_job, BOOKED);
    let second = peer_of("hosts_rows_marks_b", &first);

    let body = listed(&second).await;
    let plain = entry(&body, "host-n");
    assert_eq!(plain["phase"], "memory-pressure");
    assert_eq!(plain["capabilities"], json!({"arch": "x86_64"}));
    assert_eq!(plain["labels"], json!({"zone": "z1"}));
    let drained = entry(&body, "host-d");
    assert_eq!(
        (&drained["mark"], &drained["releasing"]),
        (&json!("draining"), &json!(false))
    );
    let released = entry(&body, "host-r");
    assert_eq!(
        (&released["mark"], &released["releasing"]),
        (&json!("releasing"), &json!(true))
    );
    let wanted = json!({ "reason": "host-job", "run_id": "job-t" });
    assert_eq!(entry(&body, "host-t")["taint"], wanted);
    assert_eq!(entry(&body, "host-t")["state"], "live");

    first.advance(second.agents.registry.longest_grace() + Duration::from_millis(1));
    let body = listed(&second).await;
    let lost = entry(&body, "host-n");
    assert_eq!(
        (&lost["state"], &lost["lost"]),
        (&json!("lost"), &json!(true))
    );
}

/// Asserts that the listing of a server holding no job tables is its registry's hosts alone,
/// and that the listing of one whose rows cannot be read is its registry's hosts with the
/// failure logged, not an error. Catches a listing that fails or lists nothing when the store
/// is gone, and one that does not say so.
#[tokio::test]
async fn the_listing_falls_back_to_the_registry_without_tables_or_rows() {
    let mut without = rig("hosts_rows_none", false);
    without.register("host-a", true);
    assert_eq!(ids(&listed(&without).await), ["host-a"]);

    process_sink();
    let lines = Lines::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(lines.clone())
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let mut with = clocked_rig("hosts_rows_down", false, NEVER);
    with.register("host-b", true);
    with.store.fail_clock(true);
    assert_eq!(ids(&listed(&with).await), ["host-b"]);
    let text = lines.take();
    let said = "the hosts' rows could not be read: only this server's hosts are listed";
    assert!(text.contains(said), "{text}");
}

/// Asserts that the job service's hosts source is the listing with the rows: `Hosts::hosts` of
/// the shared state lists the host only a row knows. Catches a job service that lists the
/// registry's hosts while `GET /v1/hosts` lists the rows too.
#[tokio::test]
async fn the_job_service_lists_the_same_hosts() {
    let mut first = clocked_rig("hosts_rows_svc", false, NEVER);
    first.join("host-a", false).await;
    let second = peer_of("hosts_rows_svc_b", &first);
    let list = Hosts::hosts(&shared_over(&second)).await;
    let listed: Vec<String> = list.hosts.iter().map(|host| host.id.to_string()).collect();
    assert_eq!(listed, ["host-a"]);
}
