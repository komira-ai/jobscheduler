//! A host's session is a row of the store, so any server over the store serves any heartbeat of
//! any host: two servers over one Postgres store, a host that said hello to one and beats to
//! the other, and a beat of the host that is delivered late. The agents are fakes that send the
//! per-job messages and a beat sequence. Nothing waits on time: the clocks and the ticks are
//! the harness's.

use std::collections::BTreeMap;
use std::ffi::OsString;

use js_state::postgres::test_database;
use serde_json::{Value, json};
use tokio::runtime::Runtime;

use crate::agents_support::{JOIN, Session, job_hello, job_host_args, post};
use crate::jobs_support::{JobBody, commands, exited, job_agent, job_state, running, submit_body};
use crate::store_support::store_file;
use crate::support::{Harness, Reply, Scratch};

/// The pool of [`crate::agents_support::JOB_POOL`] with room for three machines.
const POOL: &str = r#"
name = "burst"
class = "burst-host"
max_machines = 3

[shape]
cpu_millis = 8000
memory_bytes = 17179869184

[capabilities]
arch = "x86_64"
"#;

/// The flags of a server whose hosts run jobs, in [`POOL`], over the Postgres store at `url`.
fn over_postgres(scratch: &Scratch, url: &str) -> Vec<OsString> {
    let mut args = job_host_args(scratch);
    scratch.write("agent-pool.toml", POOL, 0o644);
    args.push("--store".into());
    args.push(store_file(scratch, url).into_os_string());
    args
}

/// A server named `name` over the Postgres store at `url`.
async fn server(url: &str, name: &str) -> Harness {
    let scratch = Scratch::new(name);
    let args = over_postgres(&scratch, url);
    Harness::with(scratch, args).await
}

/// The heartbeat of `session` to `server`, with the sequence `seq` and the listing `jobs`.
async fn beat(server: &Harness, session: &Session, seq: u64, jobs: &[Value]) -> Reply {
    let mut body = session.beat();
    body["seq"] = json!(seq);
    body["jobs"] = json!(jobs);
    post(server, "/v1/agent/heartbeat", Some(JOIN), &body).await
}

/// [`beat`], which must be answered 200: the reply's JSON.
async fn served(server: &Harness, session: &Session, seq: u64, jobs: &[Value]) -> Value {
    let reply = beat(server, session, seq, jobs).await;
    assert_eq!(reply.status, 200, "seq {seq}: {}", reply.body);
    reply.json()
}

/// Submits the job `id`, which needs 3 of a host's 8 cores, to `server`.
async fn submit(server: &Harness, id: &str) {
    let made = submit_body(server, &JobBody::new(id).needs(3000, 1 << 30).json()).await;
    assert_eq!(made.status, 201, "{id}: {}", made.body);
}

/// The verdicts of `reply` on listed results, by job.
fn acks(reply: &Value) -> BTreeMap<String, String> {
    let acks = reply["job_acks"].as_array().into_iter().flatten();
    let each = acks.map(|ack| (ack["job_id"].as_str(), ack["ack"].as_str()));
    each.map(|(job, ack)| (job.unwrap().to_owned(), ack.unwrap().to_owned()))
        .collect()
}

/// Asserts that a host that said hello to one server is served by the other with no second
/// hello: the first server places a job, the host's beats go to the second, which answers 200
/// and sends the start under the allocation the first booked, the job runs on what the host
/// lists to either server, and the host's result is accepted by the first server under the
/// allocation the second named. It catches a server that refuses a session another server
/// took (409 `unknown-host`, which makes the agent say hello again and lose what it runs), a
/// start built under another allocation than the placement's, and a listing that moves a job
/// out of the state its host reported.
#[test]
fn a_beat_that_lands_on_another_server_than_the_hello_is_served_as_it_is() {
    let database = test_database();
    let url = database.url();
    Runtime::new().unwrap().block_on(async {
        let first = server(&url, "beat_rows_hello_first").await;
        let second = server(&url, "beat_rows_hello_second").await;
        let session = job_agent(&first, &job_hello("burst-1")).await;
        submit(&first, "job-1").await;
        first.ticks.step().await;
        assert_eq!(job_state(&first, "job-1").await, "JOB_STATE_PLACED");

        // The host's first beat reaches the second server, which sends the placed job's start.
        let reply = served(&second, &session, 1, &[]).await;
        let (starts, stops) = commands(&reply);
        assert_eq!(stops, Vec::<String>::new(), "{reply}");
        assert_eq!(starts.len(), 1, "{reply}");
        let (job, allocation) = starts.into_iter().next().unwrap();
        assert_eq!(job, "job-1");
        assert_eq!(job_state(&first, "job-1").await, "JOB_STATE_PLACED");

        // It runs the job; its beats go to either server, and the job stays running, with no
        // start sent again and no stop.
        let listing = [running("job-1")];
        for (seq, to) in [(2, &second), (3, &first), (4, &second), (5, &first)] {
            let reply = served(to, &session, seq, &listing).await;
            assert_eq!(commands(&reply), (Vec::new(), Vec::new()), "{reply}");
            assert_eq!(job_state(&first, "job-1").await, "JOB_STATE_RUNNING");
            assert_eq!(job_state(&second, "job-1").await, "JOB_STATE_RUNNING");
        }

        // The result under the allocation the second server named is accepted by the first.
        let ended = [exited("job-1", &allocation)];
        let reply = served(&first, &session, 6, &ended).await;
        assert_eq!(
            acks(&reply),
            BTreeMap::from([("job-1".into(), "ACCEPTED".into())])
        );
        assert_eq!(job_state(&second, "job-1").await, "JOB_STATE_SUCCEEDED");
        let reply = served(&second, &session, 7, &[]).await;
        assert_eq!(commands(&reply), (Vec::new(), Vec::new()), "{reply}");
    });
}

/// Asserts that a beat of a host that is delivered late, after a newer beat of the same host
/// was applied (to another server), changes nothing. A newer listing ends one job and holds
/// another; the older listing, which reaches the second server afterwards, omits the ended
/// job's container and lists the running job as ended. It is answered 200 with no verdict on
/// any result, the running job stays running, the ended job's allocation is still held (a
/// queued job that needs its cores stays queued), and the next beat of the host that omits
/// the container frees it, so the queued job is placed. It catches a late listing that
/// applies: one that frees a container the newer listing still held (the cores booked again
/// while the host holds the container), one that ends a job the host runs, and one whose
/// results are answered as if it were the newest.
#[test]
fn a_late_beat_changes_no_job_and_frees_no_container() {
    let database = test_database();
    let url = database.url();
    Runtime::new().unwrap().block_on(async {
        let first = server(&url, "beat_rows_late_first").await;
        let second = server(&url, "beat_rows_late_second").await;
        let session = job_agent(&first, &job_hello("burst-1")).await;
        submit(&first, "job-1").await;
        submit(&first, "job-2").await;
        first.ticks.step().await;
        let mut allocations = BTreeMap::new();
        for seq in [1, 2] {
            let reply = served(&first, &session, seq, &[]).await;
            allocations.extend(commands(&reply).0);
        }
        assert_eq!(allocations.len(), 2, "{allocations:?}");
        let both = [running("job-1"), running("job-2")];
        served(&first, &session, 3, &both).await;
        assert_eq!(job_state(&first, "job-2").await, "JOB_STATE_RUNNING");

        // A third job needs cores the two running ones hold: it waits.
        submit(&first, "job-3").await;
        first.ticks.step().await;
        assert_eq!(job_state(&first, "job-3").await, "JOB_STATE_QUEUED");

        // The newer beat (sequence 5): the second job ended, its result is accepted.
        let newer = [running("job-1"), exited("job-2", &allocations["job-2"])];
        let reply = served(&first, &session, 5, &newer).await;
        assert_eq!(
            acks(&reply),
            BTreeMap::from([("job-2".into(), "ACCEPTED".into())])
        );
        assert_eq!(job_state(&first, "job-2").await, "JOB_STATE_SUCCEEDED");

        // The older beat (sequence 4) reaches the second server now.
        let older = [exited("job-1", &allocations["job-1"])];
        let reply = served(&second, &session, 4, &older).await;
        assert_eq!(acks(&reply), BTreeMap::new(), "{reply}");
        assert_eq!(job_state(&second, "job-1").await, "JOB_STATE_RUNNING");
        assert_eq!(job_state(&second, "job-2").await, "JOB_STATE_SUCCEEDED");
        first.ticks.step().await;
        second.ticks.step().await;
        assert_eq!(
            job_state(&first, "job-3").await,
            "JOB_STATE_QUEUED",
            "the container of job-2 is still held"
        );

        // The host's next beat omits the container (its result was acknowledged): freed.
        served(&second, &session, 6, &[running("job-1")]).await;
        first.ticks.step().await;
        assert_eq!(job_state(&first, "job-3").await, "JOB_STATE_PLACED");
        assert_eq!(job_state(&first, "job-1").await, "JOB_STATE_RUNNING");
    });
}
