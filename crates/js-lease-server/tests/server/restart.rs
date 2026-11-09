//! A start over the lease file of an earlier release, which is ignored, and a restart over a
//! job store restored from an older backup; and the job path, which has no lease and no time
//! to live to keep.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use js_jobs::{JobStore, ScriptedIds};
use js_state::sqlite::{Options, SqliteStore};
use js_state::{ManualClock as StoreClock, SystemClock};
use serde_json::json;

use crate::agents_support::{JOB_POOL, JOIN, Session, entry, hosts, listed};
use crate::jobs_support::{
    beat_with, commands, job_agent as shared_agent, job_of, one_start, running, submit_body,
};
use crate::support::{
    Harness, Scratch, T0, TOKEN, base_args, config, lease_file, replace, request,
};

/// A lease file the previous release could have left: a held lease on `gpu-01`, a releasing
/// one on `gpu-02` and a pending one with no host.
fn left_over() -> String {
    lease_file(&[
        (1, "held", Some("gpu-01")),
        (2, "releasing", Some("gpu-02")),
        (3, "pending", None),
    ])
}

/// Asserts that a lease file the previous release left at the state file's path is ignored
/// under both values of `--boot`: the server starts, lists `gpu-01` and `gpu-02` with no
/// lease (the leases the file holds are not seated), and after a tick and an orderly stop
/// the file is byte for byte what it was (it is not read for its content, locked, rewritten
/// or removed). A rollback to the previous release reads it as it was left. It catches a
/// server that restores the file's bookings, that rewrites or deletes it at a tick or a
/// stop, or that refuses a start over it.
#[tokio::test]
async fn a_lease_file_left_by_the_previous_release_is_ignored_and_left_as_it_was() {
    for boot in ["initialise", "resume"] {
        let scratch = Scratch::new(&format!("leftover_{boot}"));
        std::fs::write(scratch.state(), left_over()).unwrap();
        let args = replace(base_args(&scratch), "--boot", boot);
        let h = Harness::with(scratch, args, None).await;
        let listing = hosts(&h).await;
        assert_eq!(listed(&listing), ["gpu-01", "gpu-02"], "{boot}");
        for id in ["gpu-01", "gpu-02"] {
            let host = entry(&listing, id);
            assert!(host.get("lease").is_none(), "{boot}: {host}");
            assert!(host.get("leases").is_none(), "{boot}: {host}");
        }
        h.clock.advance(Duration::from_secs(7201));
        h.ticks.step().await;
        let state = h.scratch.state();
        h.running.shutdown().await.unwrap();
        assert_eq!(
            std::fs::read_to_string(state).unwrap(),
            left_over(),
            "{boot}"
        );
    }
}

/// Asserts that a lease file that is no lease file (bytes no TOML reader takes) at the
/// state file's path does not refuse a start under `--boot resume`, and is left as it was.
/// It catches a server that parses the file to start.
#[tokio::test]
async fn a_lease_file_that_cannot_be_read_does_not_refuse_a_start() {
    let scratch = Scratch::new("leftover_garbage");
    let garbage = b"\xff\xfe not toml at all = = =".as_slice();
    std::fs::write(scratch.state(), garbage).unwrap();
    let args = replace(base_args(&scratch), "--boot", "resume");
    let h = Harness::with(scratch, args, None).await;
    assert_eq!(listed(&hosts(&h).await), ["gpu-01", "gpu-02"]);
    let state = h.scratch.state();
    h.running.shutdown().await.unwrap();
    assert_eq!(std::fs::read(state).unwrap(), garbage);
}

/// Asserts that `--boot resume` with no lease file at the state file's path starts, lists the
/// pool file's hosts, and makes no lease file. It catches a server that asks for the file
/// `--boot resume` used to require, and one that makes it.
#[tokio::test]
async fn resume_without_a_lease_file_starts_and_makes_none() {
    let scratch = Scratch::new("resume_no_file");
    let args = replace(base_args(&scratch), "--boot", "resume");
    let h = Harness::with(scratch, args, None).await;
    assert_eq!(listed(&hosts(&h).await), ["gpu-01", "gpu-02"]);
    let state = h.scratch.state();
    h.ticks.step().await;
    h.running.shutdown().await.unwrap();
    assert!(!state.exists());
}

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

/// The job store's file beside the state file of `scratch`.
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
    let made = submit_body(h, &body).await;
    assert_eq!(made.status, 201, "{}", made.body);
    made.json()["job"]["id"].as_str().unwrap().to_owned()
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
    shared_agent(h, &hello).await
}

/// Every job of the job store in `file` with its allocation, read after the server stopped,
/// one line each, sorted: `<job id> <state> [<allocation id> <host> <step>]`.
pub(crate) fn stored(file: &Path) -> Vec<String> {
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

/// Asserts that the job path works with no ttl flag given: a job is submitted through the job service, placed on a registered
/// agent (its start names the job and the agent lists it running with nothing told
/// back), a second job submitted while the first holds the host is queued and then
/// cancelled (200 `JOB_STATE_CANCELLED`) and is never started, and the first job's
/// success, reported by the agent, is accepted (`ACCEPTED`), after which the job reads
/// `JOB_STATE_SUCCEEDED` and its result is served. It catches a ttl setting that reaches
/// into job submission, placement, results or cancel.
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
