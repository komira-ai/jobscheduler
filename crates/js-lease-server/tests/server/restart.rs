//! A restart over the same lease file, and over a job store restored from an older backup;
//! and the job path, which has no lease and no time to live to keep.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use js_jobs::{JobStore, ScriptedIds};
use js_pool_static::EXAMPLE_CONFIG;
use js_state::sqlite::{Options, SqliteStore};
use js_state::{ManualClock as StoreClock, SystemClock};
use serde_json::{Value, json};

use crate::agents_support::{JOIN, Session, post, session};
use crate::support::{
    Harness, LogCapture, Reply, Scratch, SharedPool, T0, TOKEN, args_with, base_args, config, ids,
    lease_body, lease_file, replace, request,
};

/// Asserts that, over a provider that outlives both servers (a service of its own), a
/// server started again with `--boot resume` over the lease file of one
/// that stopped keeps the leases (the provider, which outlives both, still has them
/// booked), never reissues an id of the first run, and places a new lease on the host
/// left free.
#[tokio::test]
async fn resume_keeps_leases_and_never_reuses_ids() {
    let pool = SharedPool::new();
    let scratch = Scratch::new("resume");
    let args = base_args(&scratch);
    let first = Harness::with(scratch, args, Some(Box::new(pool.clone()))).await;
    let granted = first.call("POST", "/v1/leases", Some(&lease_body())).await;
    assert_eq!(granted.json()["id"], "1-1");
    let Harness {
        running, scratch, ..
    } = first;
    running.shutdown().await.unwrap();

    let args = replace(base_args(&scratch), "--boot", "resume");
    let second = Harness::with(scratch, args, Some(Box::new(pool))).await;
    let listed = second.call("GET", "/v1/leases", None).await;
    assert_eq!(ids(&listed), vec!["1-1"]);
    assert_eq!(listed.json()["leases"][0]["state"], "held");
    assert_eq!(listed.json()["leases"][0]["host"], "gpu-01");

    let next = second.call("POST", "/v1/leases", Some(&lease_body())).await;
    assert_eq!(next.status, 201, "{}", next.body);
    assert_eq!(next.json()["id"], "2-2");
    assert_eq!(next.json()["host"], "gpu-02");

    assert_eq!(
        second.call("DELETE", "/v1/leases/1-1", None).await.json(),
        json!({ "state": "freed" })
    );
}

/// Asserts that a first boot over an existing lease file loads it like a resume.
#[tokio::test]
async fn initialise_over_an_existing_file_loads_it() {
    let pool = SharedPool::new();
    let scratch = Scratch::new("initialise_existing");
    let args = args_with(&scratch, &[]);
    let first = Harness::with(scratch, args.clone(), Some(Box::new(pool.clone()))).await;
    first.call("POST", "/v1/leases", Some(&lease_body())).await;
    let Harness {
        running, scratch, ..
    } = first;
    running.shutdown().await.unwrap();

    let second = Harness::with(scratch, args, Some(Box::new(pool))).await;
    assert_eq!(
        ids(&second.call("GET", "/v1/leases", None).await),
        vec!["1-1"]
    );
}

/// Asserts that a server whose lease file has used its last sequence answers an acquire
/// 503 `ids-exhausted`, books nothing and leaves the file as it was: the table can issue
/// no lease until the operator restores the state file.
#[tokio::test]
async fn an_acquire_after_the_last_lease_id_is_503() {
    let scratch = Scratch::new("ids_exhausted");
    let head = format!(
        "version = 1\nterm = 1\nnext_sequence = {}\nlease_count = 0\n",
        i64::MAX
    );
    std::fs::write(scratch.state(), &head).unwrap();
    let args = replace(base_args(&scratch), "--boot", "resume");
    let h = Harness::with(scratch, args, None).await;
    let refused = h.call("POST", "/v1/leases", Some(&lease_body())).await;
    assert_eq!(refused.status, 503, "{}", refused.body);
    assert_eq!(refused.json(), json!({ "error": "ids-exhausted" }));
    assert_eq!(
        ids(&h.call("GET", "/v1/leases", None).await),
        Vec::<String>::new()
    );
}

/// Stops `first`, a server on the base flags, and starts another over its scratch
/// directory with `--boot resume` and the pool file in its place, exactly as the program
/// does: the second server builds a fresh pool from the file, and no provider outlives
/// the first.
async fn restarted(first: Harness) -> Harness {
    let Harness {
        running, scratch, ..
    } = first;
    running.shutdown().await.unwrap();
    let args = replace(base_args(&scratch), "--boot", "resume");
    Harness::with(scratch, args, None).await
}

/// Asserts that a restart with the shipped provider, a pool built again from the pool
/// file, keeps a held lease held on the same host (the lease file is the record of the
/// pool's bookings), that the host is not leased to anyone else (the second host is
/// taken by the next acquire and the third acquire has no capacity), that the lease can
/// be released, and that its host is free after the release.
#[tokio::test]
async fn a_restart_keeps_a_held_lease_on_the_shipped_pool() {
    let first = Harness::new("resume_shipped").await;
    let granted = first.call("POST", "/v1/leases", Some(&lease_body())).await;
    assert_eq!(granted.json()["host"], "gpu-01");
    let second = restarted(first).await;

    let listed = second.call("GET", "/v1/leases", None).await.json();
    assert_eq!(listed["leases"][0]["id"], "1-1");
    assert_eq!(listed["leases"][0]["state"], "held");
    assert_eq!(listed["leases"][0]["host"], "gpu-01");

    let next = second.call("POST", "/v1/leases", Some(&lease_body())).await;
    assert_eq!(next.json()["host"], "gpu-02");
    let full = second.call("POST", "/v1/leases", Some(&lease_body())).await;
    assert_eq!(full.status, 409, "{}", full.body);
    assert_eq!(full.json(), json!({ "error": "no-capacity" }));

    let released = second.call("DELETE", "/v1/leases/1-1", None).await;
    assert_eq!(released.json(), json!({ "state": "freed" }));
    let again = second.call("POST", "/v1/leases", Some(&lease_body())).await;
    assert_eq!(again.json()["host"], "gpu-01");
}

/// Asserts that a lease file with a held, a releasing and a pending lease without a
/// host restores both hosts as booked (an acquire has no capacity), and that a tick then
/// settles the other two as the table always does: the releasing lease is stopped and
/// frees its host, the pending lease is not known to the pool and is dropped.
#[tokio::test]
async fn every_booking_of_the_file_is_seated_before_the_table_looks() {
    let scratch = Scratch::new("resume_states");
    let leases = [
        (1, "held", Some("gpu-01")),
        (2, "releasing", Some("gpu-02")),
        (3, "pending", None),
    ];
    std::fs::write(scratch.state(), lease_file(&leases)).unwrap();
    let args = replace(base_args(&scratch), "--boot", "resume");
    let h = Harness::with(scratch, args, None).await;
    assert_eq!(ids(&h.call("GET", "/v1/leases", None).await).len(), 3);
    let full = h.call("POST", "/v1/leases", Some(&lease_body())).await;
    assert_eq!(full.json(), json!({ "error": "no-capacity" }));

    h.ticks.step().await;
    assert_eq!(ids(&h.call("GET", "/v1/leases", None).await), vec!["1-1"]);
    let next = h.call("POST", "/v1/leases", Some(&lease_body())).await;
    assert_eq!(next.json()["host"], "gpu-02");
}

/// Asserts that a lease whose host the pool file no longer lists is dropped at the
/// restart (the table finds the pool ignorant of it), that the drop is logged with the
/// lease, the host and the reason, and that the hosts left are leasable.
#[tokio::test]
async fn a_host_the_pool_file_dropped_loses_its_lease_and_says_so() {
    let first = Harness::new("resume_unlisted").await;
    first.call("POST", "/v1/leases", Some(&lease_body())).await;
    let only_gpu_02 = format!(
        "[[host]]{}",
        EXAMPLE_CONFIG.rsplit("[[host]]").next().unwrap()
    );
    let Harness {
        running, scratch, ..
    } = first;
    running.shutdown().await.unwrap();

    let logs = LogCapture::start();
    let args = replace(base_args(&scratch), "--boot", "resume");
    std::fs::write(scratch.path("pool.toml"), &only_gpu_02).unwrap();
    let h = Harness::with(scratch, args, None).await;
    let text = logs.text();
    assert!(text.contains("1-1"), "{text}");
    assert!(text.contains("gpu-01"), "{text}");
    assert!(text.contains("not in the pool"), "{text}");

    assert!(ids(&h.call("GET", "/v1/leases", None).await).is_empty());
    let next = h.call("POST", "/v1/leases", Some(&lease_body())).await;
    assert_eq!(next.json()["host"], "gpu-02");
}

/// Asserts that when a lease file names one host for two leases the lower id keeps it,
/// the other is dropped, and the drop is logged with the reason.
#[tokio::test]
async fn two_leases_on_one_host_leave_the_lower_id_seated() {
    let scratch = Scratch::new("resume_double");
    let leases = [(1, "held", Some("gpu-01")), (2, "held", Some("gpu-01"))];
    std::fs::write(scratch.state(), lease_file(&leases)).unwrap();
    let logs = LogCapture::start();
    let args = replace(base_args(&scratch), "--boot", "resume");
    let h = Harness::with(scratch, args, None).await;
    let text = logs.text();
    assert!(text.contains("1-2"), "{text}");
    assert!(text.contains("held by lease 1-1"), "{text}");
    assert_eq!(ids(&h.call("GET", "/v1/leases", None).await), vec!["1-1"]);
}

/// A pool of up to two machines whose capabilities name an `arch`, as `js-sched` needs to
/// place a job.
const JOB_POOL: &str = r#"
name = "burst"
class = "burst-host"
max_machines = 2

[shape]
cpu_millis = 8000
memory_bytes = 17179869184

[capabilities]
arch = "x86_64"
"#;

/// The image every job of the restore runs: pinned by digest, as the service demands.
const IMAGE: &str =
    "team/app@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

/// The flags of a server with agent registration over [`JOB_POOL`], the job service on the
/// loopback, `js-sched` placing and `boot` as its boot.
fn job_args(scratch: &Scratch, boot: &str) -> Vec<OsString> {
    let mut args = replace(base_args(scratch), "--boot", boot);
    let tokens = scratch.write("agent-tokens", &format!("{JOIN}\n"), 0o600);
    let pool = scratch.write("agent-pool.toml", JOB_POOL, 0o644);
    let flags = [
        ("--agent-token-file", tokens.into_os_string()),
        ("--agent-pool-config", pool.into_os_string()),
        ("--job-listen", "127.0.0.1:0".into()),
        ("--placement", "js-sched".into()),
    ];
    for (flag, value) in flags {
        args.push(flag.into());
        args.push(value);
    }
    args
}

/// A server over `scratch` booted with `boot`, its job tables stamped by `clock` and their
/// ids drawn from `ids`.
async fn job_server(
    scratch: Scratch,
    boot: &str,
    clock: &Arc<StoreClock>,
    ids: Arc<ScriptedIds>,
) -> Harness {
    let checked = config(job_args(&scratch, boot)).unwrap();
    let store_clock: Arc<dyn js_state::Clock> = clock.clone();
    Harness::over(scratch, checked.with_job_sources(store_clock, ids)).await
}

/// The job store's file beside the lease file of `scratch`.
fn jobs_file(scratch: &Scratch) -> PathBuf {
    scratch.path("leases.toml.jobs")
}

/// Submits a job that needs 6 of a host's 8 cores, so no host runs two; returns its id.
async fn submit(h: &Harness) -> String {
    let body = json!({
        "spec": {
            "image": IMAGE,
            "needs": { "cpuMillis": "6000", "memoryBytes": "1073741824" },
            "wallLimitSeconds": "600",
        }
    });
    let bytes = body.to_string().into_bytes();
    let addr = h.running.job_addr().expect("the job service is on");
    let made = request(addr, "POST", "/v1/jobs", Some(TOKEN), Some(&bytes)).await;
    assert_eq!(made.status, 201, "{}", made.body);
    made.json()["job"]["id"].as_str().unwrap().to_owned()
}

/// The job service's answer about the job `id`.
async fn job_of(h: &Harness, id: &str) -> Reply {
    let addr = h.running.job_addr().expect("the job service is on");
    request(addr, "GET", &format!("/v1/jobs/{id}"), Some(TOKEN), None).await
}

/// The hello of `host`, an agent that speaks the per-job messages, admitted.
async fn job_agent(h: &Harness, host: &str) -> Session {
    let hello = json!({
        "host_id": host,
        "pool": "burst",
        "agent_version": "0.1.0",
        "shape": { "cpu_millis": 8000, "memory_bytes": 17179869184u64, "gpus": 0 },
        "capabilities": { "per-job": "true", "arch": "x86_64" },
        "phase": "serving",
    });
    session(&post(h, "/v1/agent/hello", Some(JOIN), &hello).await, host)
}

/// The heartbeat of `agent` listing `jobs` (each a `(job id, state, result)`), answered 200.
async fn beat_with(h: &Harness, agent: &Session, jobs: &[Value]) -> Value {
    let mut body = agent.beat();
    body["jobs"] = json!(jobs);
    let reply = post(h, "/v1/agent/heartbeat", Some(JOIN), &body).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    reply.json()
}

/// A job `id` the agent lists as running.
fn running(id: &str) -> Value {
    json!({ "job_id": id, "state": "running" })
}

/// The `(job id, allocation id)` of each start in `reply`, and the job ids it stops.
fn commands(reply: &Value) -> (Vec<(String, String)>, Vec<String>) {
    let mut starts = Vec::new();
    let mut stops = Vec::new();
    for command in reply["commands"].as_array().into_iter().flatten() {
        let job = command["job_id"].as_str().unwrap_or_default().to_owned();
        match command["type"].as_str() {
            Some("job_start") => {
                let allocation = command["allocation_id"].as_str().unwrap().to_owned();
                starts.push((job, allocation));
            }
            Some("job_stop") => stops.push(job),
            _ => {}
        }
    }
    (starts, stops)
}

/// The one start `reply` carries.
fn one_start(reply: &Value) -> (String, String) {
    let (starts, stops) = commands(reply);
    assert!(stops.is_empty(), "{reply}");
    assert_eq!(starts.len(), 1, "{reply}");
    starts.into_iter().next().unwrap()
}

/// Every job of the job store in `file` with its allocation, read after the server stopped,
/// one line each, sorted: `<job id> <state> [<allocation id> <host> <step>]`.
fn stored(file: &Path) -> Vec<String> {
    let clock = Arc::new(SystemClock::default());
    let store = SqliteStore::open(file, clock, Options::default()).unwrap();
    let jobs = JobStore::new(Arc::new(store), ScriptedIds::new([0xee; 16], [])).unwrap();
    let page = jobs
        .list_jobs(None, None, NonZeroUsize::new(100).unwrap())
        .unwrap();
    let rows = page.jobs.into_iter().map(|job| {
        let allocation = jobs.get_allocation(&job.id).unwrap().map(|allocation| {
            let id = allocation.allocation_id;
            format!(" {id} {} {:?}", allocation.host, allocation.step)
        });
        format!(
            "{} {:?}{}",
            job.id,
            job.state,
            allocation.unwrap_or_default()
        )
    });
    let mut rows: Vec<String> = rows.collect();
    rows.sort();
    rows
}

/// Asserts what a restore of an older snapshot of the job store does under agents that kept
/// running. Job A is placed on `host-1`, the store is backed up online
/// (`js_state::sqlite::backup`), and job B is then placed on `host-2`; the server stops, the
/// snapshot is restored over the store offline (`js_state::sqlite::restore`), and the server
/// starts again (`--boot resume`) while both agents still hold their jobs. Then:
///
/// - Unknown jobs are stopped: `host-2`, which lists B (unknown to the restored store), is
///   told to stop it on every reply while it lists it, its killed result is acknowledged
///   `STALE`, and B stays unknown (404, no row). `host-1`, which lists A, is told nothing,
///   and A keeps its allocation.
/// - Nothing is booked twice: the next job, C, is not placed on `host-1`, whose booking for
///   A is rebuilt from the restored table, nor on `host-2` while it still lists B; it is
///   placed on `host-2` once a listing of it is clean, and the store then holds one live
///   allocation per host.
/// - No id issued before the restore is issued again: the id source's first draw after the
///   restart is A's id, which the restored store holds, and the store draws again; C's job
///   and allocation ids are none of A's or B's. B's ids are not in the restored store, so
///   nothing there could refuse them; they stay unique because the ids come from a source a
///   restore does not wind back (in a service, 128 random bits), never from the store.
///
/// It catches a restore that leaves an unknown job running, a job revived from a host's
/// listing, a booking not rebuilt from the restored table (C placed beside A), a start sent
/// to a host that still runs a job the store forgot, and a minted id written over a row the
/// restored store holds. (That the service's ids come from a source a restore does not wind
/// back is not shown here: the test gives the ids.)
#[tokio::test]
async fn a_restored_older_job_store_stops_unknown_jobs_and_reissues_no_id() {
    let scratch = Scratch::new("restore_jobs");
    // The job tables' clock reads what the server's does, so no deadline is judged across
    // two clocks.
    let clock = Arc::new(StoreClock::default());
    clock.advance(Duration::from_millis(T0));
    let (job_a, alloc_a, job_b, alloc_b) = ([1; 16], [2; 16], [3; 16], [4; 16]);
    let first_ids = ScriptedIds::new(job_a, [alloc_a, job_b, alloc_b, [0xf0; 16]]);
    let first = job_server(scratch, "initialise", &clock, Arc::new(first_ids)).await;

    let host_1 = job_agent(&first, "host-1").await;
    let a = submit(&first).await;
    first.ticks.step().await;
    let (started, a_allocation) = one_start(&beat_with(&first, &host_1, &[]).await);
    assert_eq!(started, a);
    let quiet = beat_with(&first, &host_1, &[running(&a)]).await;
    assert_eq!(commands(&quiet), (vec![], vec![]), "{quiet}");

    let snapshot = first.scratch.path("snapshot.jobs");
    let summary = js_state::sqlite::backup(&jobs_file(&first.scratch), &snapshot).unwrap();
    assert!(summary.keys > 0, "{summary:?}");

    let host_2 = job_agent(&first, "host-2").await;
    let b = submit(&first).await;
    first.ticks.step().await;
    let (started, b_allocation) = one_start(&beat_with(&first, &host_2, &[]).await);
    assert_eq!(started, b);
    beat_with(&first, &host_2, &[running(&b)]).await;
    let issued: BTreeSet<String> = [&a, &a_allocation, &b, &b_allocation]
        .into_iter()
        .cloned()
        .collect();
    assert_eq!(issued.len(), 4, "{issued:?}");

    let Harness {
        running: server,
        scratch,
        ..
    } = first;
    server.shutdown().await.unwrap();
    js_state::sqlite::restore(&snapshot, &jobs_file(&scratch)).unwrap();
    let restored = stored(&jobs_file(&scratch));
    assert_eq!(
        restored,
        [format!("{a} Running {a_allocation} host-1 Booked")]
    );

    // The next draw is A's id, which the restored store holds; then fresh ids.
    let second_ids = ScriptedIds::new(job_a, [[5; 16], [6; 16], [0xf1; 16]]);
    let h = job_server(scratch, "resume", &clock, Arc::new(second_ids)).await;
    let host_1 = job_agent(&h, "host-1").await;
    let host_2 = job_agent(&h, "host-2").await;

    let told = beat_with(&h, &host_1, &[running(&a)]).await;
    assert_eq!(commands(&told), (vec![], vec![]), "{told}");
    let told = beat_with(&h, &host_2, &[running(&b)]).await;
    assert_eq!(commands(&told), (vec![], vec![b.clone()]), "{told}");
    // Told again while it still lists the job.
    let again = beat_with(&h, &host_2, &[running(&b)]).await;
    assert_eq!(commands(&again).1, vec![b.clone()], "{again}");

    let c = submit(&h).await;
    assert_eq!(c, "05".repeat(16), "the draw of A's id was drawn again");
    assert!(!issued.contains(&c), "{c} was issued before the restore");
    // While host-2 still runs B, nothing is placed: host-1 is full with A, and host-2's
    // capacity is not counted until its listing is clean.
    h.ticks.step().await;
    let busy = beat_with(&h, &host_2, &[running(&b)]).await;
    assert_eq!(commands(&busy), (vec![], vec![b.clone()]), "{busy}");
    let still = beat_with(&h, &host_1, &[running(&a)]).await;
    assert_eq!(commands(&still), (vec![], vec![]), "{still}");

    let killed = json!({
        "job_id": b,
        "state": "exited_unacked",
        "result": { "job_id": b, "allocation_id": b_allocation, "outcome": "killed" },
    });
    let acked = beat_with(&h, &host_2, &[killed]).await;
    assert_eq!(
        acked["job_acks"],
        json!([{ "job_id": b, "ack": "STALE" }]),
        "{acked}"
    );
    assert_eq!(commands(&acked), (vec![], vec![b.clone()]), "{acked}");
    h.ticks.step().await;
    let clean = beat_with(&h, &host_2, &[]).await;
    assert_eq!(commands(&clean), (vec![], vec![]), "{clean}");
    let (started, c_allocation) = one_start(&beat_with(&h, &host_2, &[]).await);
    assert_eq!(started, c);
    assert_eq!(c_allocation, "06".repeat(16));
    assert!(
        !issued.contains(&c_allocation),
        "{c_allocation} was issued before"
    );
    assert_eq!(job_of(&h, &b).await.status, 404);

    let Harness {
        running: server,
        scratch,
        ..
    } = h;
    server.shutdown().await.unwrap();
    let mut expected = vec![
        format!("{a} Running {a_allocation} host-1 Booked"),
        format!("{c} Placed {c_allocation} host-2 Booked"),
    ];
    expected.sort();
    assert_eq!(stored(&jobs_file(&scratch)), expected);
}

/// Asserts that the job path works with no ttl flag given, over the same server that
/// serves leases: a job is submitted through the job service, placed on a registered
/// agent (its start names the job and the agent lists it running with nothing told
/// back), a second job submitted while the first holds the host is queued and then
/// cancelled (200 `JOB_STATE_CANCELLED`) and is never started, and the first job's
/// success, reported by the agent, is accepted (`ACCEPTED`), after which the job reads
/// `JOB_STATE_SUCCEEDED` and its result is served. It catches a removal of lease renewal
/// that reaches into job submission, placement, results or cancel.
#[tokio::test]
async fn the_job_path_is_unaffected() {
    let scratch = Scratch::new("job_path");
    let clock = Arc::new(StoreClock::default());
    clock.advance(Duration::from_millis(T0));
    let ids = ScriptedIds::new([1; 16], [[2; 16], [3; 16], [0xf0; 16]]);
    let h = job_server(scratch, "initialise", &clock, Arc::new(ids)).await;
    let host = job_agent(&h, "host-1").await;

    let a = submit(&h).await;
    h.ticks.step().await;
    let (started, allocation) = one_start(&beat_with(&h, &host, &[]).await);
    assert_eq!(started, a);
    let quiet = beat_with(&h, &host, &[running(&a)]).await;
    assert_eq!(commands(&quiet), (vec![], vec![]), "{quiet}");

    let b = submit(&h).await;
    h.ticks.step().await;
    let busy = beat_with(&h, &host, &[running(&a)]).await;
    assert_eq!(commands(&busy), (vec![], vec![]), "{busy}");
    let addr = h.running.job_addr().expect("the job service is on");
    let path = format!("/v1/jobs/{b}:cancel");
    let cancelled = request(addr, "POST", &path, Some(TOKEN), Some(b"{}")).await;
    assert_eq!(cancelled.status, 200, "{}", cancelled.body);
    assert_eq!(cancelled.json()["job"]["state"], "JOB_STATE_CANCELLED");

    let done = json!({
        "job_id": a,
        "state": "exited_unacked",
        "result": { "job_id": a, "allocation_id": allocation, "outcome": "succeeded", "exit_code": 0 },
    });
    let acked = beat_with(&h, &host, &[done]).await;
    assert_eq!(
        acked["job_acks"],
        json!([{ "job_id": a, "ack": "ACCEPTED" }]),
        "{acked}"
    );
    h.ticks.step().await;
    let after = beat_with(&h, &host, &[]).await;
    assert_eq!(commands(&after), (vec![], vec![]), "{after}");

    let read = job_of(&h, &a).await;
    assert_eq!(read.status, 200, "{}", read.body);
    assert_eq!(read.json()["job"]["state"], "JOB_STATE_SUCCEEDED");
    let result = request(
        addr,
        "GET",
        &format!("/v1/jobs/{a}/result"),
        Some(TOKEN),
        None,
    )
    .await;
    assert_eq!(result.status, 200, "{}", result.body);
    assert!(result.json()["result"].is_object(), "{}", result.body);
    assert_eq!(
        job_of(&h, &b).await.json()["job"]["state"],
        "JOB_STATE_CANCELLED"
    );
}
