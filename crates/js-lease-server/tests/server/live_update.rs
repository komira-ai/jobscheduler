//! An update of a server that runs as two instances for a while over one Postgres store (an
//! orchestrator's start-first update): the second starts while the first serves every host and
//! has jobs placed and running, then the first is stopped the way a termination signal stops it
//! ([`js_lease_server::Running::hold`], then [`js_lease_server::Running::shutdown`]). The agents
//! are fakes that count what they are told; the test plays the orchestrator's routing, which
//! sends a new connection to the second server once the first drains. Nothing waits on time:
//! the clocks and ticks are the harness's.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;

use js_state::postgres::test_database;
use serde_json::{Value, json};
use tokio::runtime::Runtime;

use crate::agents_support::{JOIN, Session, entry, hosts, job_hello, job_host_args, post};
use crate::jobs_support::{
    JobBody, commands, exited, job_agent, job_state, left_the_queue, running, submit_body,
};
use crate::store_support::store_file;
use crate::support::{Harness, Scratch};

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

/// An agent that speaks the per-job messages and counts what it is told: every start it is
/// sent (by job, the allocations), how many hellos it said, and the jobs it runs and has ended.
struct Agent {
    host: String,
    session: Option<Session>,
    hellos: u32,
    /// Every allocation a start named, by job.
    starts: BTreeMap<String, BTreeSet<String>>,
    /// The jobs it runs, with the allocation each was started under.
    running: BTreeMap<String, String>,
    /// The jobs it ended whose result is not acknowledged yet.
    ended: BTreeMap<String, String>,
}

impl Agent {
    fn new(host: &str) -> Self {
        Self {
            host: host.to_owned(),
            session: None,
            hellos: 0,
            starts: BTreeMap::new(),
            running: BTreeMap::new(),
            ended: BTreeMap::new(),
        }
    }

    /// Says hello to `server`, which must admit it.
    async fn hello(&mut self, server: &Harness) {
        self.session = Some(job_agent(server, &job_hello(&self.host)).await);
        self.hellos += 1;
    }

    /// The listing of every job it holds: those it runs, and those it ended with their result.
    fn listing(&self) -> Vec<Value> {
        let runs = self.running.keys().map(|id| running(id));
        let ends = self
            .ended
            .iter()
            .map(|(id, allocation)| exited(id, allocation));
        runs.chain(ends).collect()
    }

    /// One heartbeat to `server`, as the agent sends it: its session and its whole listing,
    /// whichever server took its hello. A start it is sent is counted and run (once: a start
    /// of a job it runs or ran is counted only); an acknowledged result is let go of. Every
    /// server over the store serves the session, so any refusal (a 409 `unknown-host` or a
    /// 403 `session-mismatch` that would send the agent to say hello again) fails the test.
    async fn beat(&mut self, server: &Harness) {
        let session = self.session.as_ref().expect("the agent said hello");
        let mut body = session.beat();
        body["jobs"] = json!(self.listing());
        let reply = post(server, "/v1/agent/heartbeat", Some(JOIN), &body).await;
        assert_eq!(reply.status, 200, "{}: {}", self.host, reply.body);
        let reply = reply.json();
        let (starts, stops) = commands(&reply);
        assert_eq!(stops, Vec::<String>::new(), "{}: {reply}", self.host);
        for (job, allocation) in starts {
            self.starts
                .entry(job.clone())
                .or_default()
                .insert(allocation.clone());
            if !self.ended.contains_key(&job) {
                self.running.entry(job).or_insert(allocation);
            }
        }
        for ack in reply["job_acks"].as_array().into_iter().flatten() {
            assert_eq!(ack["ack"], "ACCEPTED", "{}: {reply}", self.host);
            self.ended.remove(ack["job_id"].as_str().unwrap());
        }
    }

    /// Ends every job it runs, with exit code 0.
    fn end_all(&mut self) {
        self.ended.append(&mut self.running);
    }
}

/// The state of each of `jobs` as `server`'s job service reports it.
async fn states(server: &Harness, jobs: &[&str]) -> Vec<String> {
    let mut seen = Vec::new();
    for job in jobs {
        seen.push(job_state(server, job).await);
    }
    seen
}

/// Submits the job `id` (3 of a host's 8 cores, so a host takes two) to `server`.
async fn submit(server: &Harness, id: &str) {
    let made = submit_body(server, &JobBody::new(id).needs(3000, 1 << 30).json()).await;
    assert_eq!(made.status, 201, "{id}: {}", made.body);
}

/// Asserts that an update during live jobs loses, doubles and strands nothing. The first
/// server places four jobs on two hosts (one host runs its two, the other has not been told
/// its two yet) and queues a fifth; the second server starts over the same store and ticks; a
/// third, empty host says hello to the first; the first is then stopped as a termination
/// signal stops it. From the signal on the first places nothing (a job submitted to it then
/// stays queued through its tick, though the third host has room), each agent's next
/// heartbeat reaches the second server, which serves the session the first took from the
/// hosts' rows (no `unknown-host`, no second hello), and the agent keeps its runs; the host
/// that had not been told its starts is told them by the second server, under the allocations
/// the first booked. By the time the first has stopped (its last tick included), the second
/// server has placed the two queued jobs on the third host (whose row it reads, though the host
/// said hello to the first), and every host ends its jobs. In the end every
/// job succeeded, each was started under one allocation on one host, each agent said hello
/// once in all, and no agent was told to stop a job. It catches a beat the second server
/// refuses because another server took the hello (the agent would say hello again and its
/// first listing there would lose what it runs), a job lost or ended at the move (a first
/// listing on the second server, the first server's last tick), a job placed twice or on two hosts,
/// a start the second server does not send for a placement the first made, a job left queued
/// or placed forever, and a draining server that goes on placing.
#[test]
fn an_update_during_live_jobs_loses_doubles_and_strands_nothing() {
    let database = test_database();
    let url = database.url();
    Runtime::new().unwrap().block_on(async {
        let first = server(&url, "live_update_first").await;
        let mut hosts = [
            Agent::new("burst-1"),
            Agent::new("burst-2"),
            Agent::new("burst-3"),
        ];
        // The first two hosts take the first four jobs; the third says hello after them.
        for host in &mut hosts[..2] {
            host.hello(&first).await;
        }
        for id in ["job-1", "job-2", "job-3", "job-4", "job-5"] {
            submit(&first, id).await;
        }
        first.ticks.step().await;
        hosts[0].beat(&first).await;
        hosts[0].beat(&first).await;
        assert_eq!(hosts[0].running.len(), 2, "the first host runs two jobs");
        let all = ["job-1", "job-2", "job-3", "job-4", "job-5", "job-6"];
        let placed = states(&first, &all[..5]).await;
        let count = |states: &[String], state: &str| states.iter().filter(|s| *s == state).count();
        assert_eq!(count(&placed, "JOB_STATE_RUNNING"), 2, "{placed:?}");
        assert_eq!(count(&placed, "JOB_STATE_PLACED"), 2, "{placed:?}");
        assert_eq!(count(&placed, "JOB_STATE_QUEUED"), 1, "{placed:?}");

        let second = server(&url, "live_update_second").await;
        second.ticks.step().await;
        assert_eq!(
            states(&second, &all[..5]).await,
            placed,
            "the second reads the same jobs"
        );
        // The third host says hello to the first server, which still serves every host.
        hosts[2].hello(&first).await;

        // The termination signal: the hold begins (and is not waited out). A job submitted
        // to the first server now stays queued through its tick, though its empty third host
        // has room: a draining server places nothing.
        {
            let hold = first.running.hold();
            tokio::pin!(hold);
            tokio::select! {
                biased;
                () = &mut hold => panic!("the hold is over at once"),
                () = std::future::ready(()) => {}
            }
        }
        submit(&first, "job-6").await;
        first.ticks.step().await;
        assert_eq!(job_state(&first, "job-6").await, "JOB_STATE_QUEUED");

        // The orchestrator sends the agents' next connections to the second server.
        for host in &mut hosts {
            host.beat(&second).await;
            host.beat(&second).await;
        }
        assert_eq!(
            hosts[1].running.len(),
            2,
            "the second host was told its starts"
        );
        first.running.shutdown().await.unwrap();
        // The second server places over every live host row, so the passes those beats poked
        // place the two queued jobs on the third host (its row is live from its hello to the
        // first server). The passes run on the second server's poller, after the beats that
        // poked them; once both jobs left the queue, the third host's next beat is told the
        // starts and the one after lists them running.
        for id in all {
            left_the_queue(&second, id).await;
        }
        hosts[2].beat(&second).await;
        hosts[2].beat(&second).await;
        let moved = states(&second, &all).await;
        assert_eq!(count(&moved, "JOB_STATE_RUNNING"), 6, "{moved:?}");
        assert_eq!(hosts[2].running.len(), 2, "{moved:?}");
        // The second server's tick changes nothing of it.
        second.ticks.step().await;
        for host in &mut hosts {
            host.beat(&second).await;
            host.beat(&second).await;
        }
        assert_eq!(
            hosts[2].running.len(),
            2,
            "the third host runs the queued jobs"
        );
        let moved = states(&second, &all).await;
        assert_eq!(count(&moved, "JOB_STATE_RUNNING"), 6, "{moved:?}");

        // The hosts end what they run, and each end is acknowledged.
        for host in &mut hosts {
            host.end_all();
            host.beat(&second).await;
        }
        second.ticks.step().await;
        let ended = states(&second, &all).await;
        assert_eq!(ended, vec!["JOB_STATE_SUCCEEDED".to_owned(); 6]);

        let mut started: BTreeMap<&str, Vec<(&str, &String)>> = BTreeMap::new();
        for host in &hosts {
            for (job, allocations) in &host.starts {
                for allocation in allocations {
                    started
                        .entry(job)
                        .or_default()
                        .push((&host.host, allocation));
                }
            }
            assert_eq!(host.hellos, 1, "{}: one hello in all", host.host);
            assert!(
                host.running.is_empty() && host.ended.is_empty(),
                "{}",
                host.host
            );
        }
        assert_eq!(started.len(), 6, "{started:?}");
        for (job, on) in &started {
            assert_eq!(
                on.len(),
                1,
                "{job} was started under one allocation: {on:?}"
            );
        }
    });
}

/// A taint an agent reports: a residue its checks found after the run `run`.
fn residue(run: &str) -> Value {
    json!({ "reason": "residue", "check": "process-as-job-user", "run_id": run })
}

/// Asserts that an agent's clear of its taint ends the taint for good when the clear reaches
/// the other server of an update: the host says hello to the first server, its taint is
/// reported to the first, which stores it, and the heartbeat of the same session that reports
/// it cleared reaches the second, which acknowledges the clear. The first is then stopped, and a third server started over the
/// store (a later restart) lists the host untainted after its hello and places a job on it. It
/// catches a clear acknowledged by a server that does not hold the taint and left in the store,
/// which every later start puts back on the host, so that it takes no job though its agent is
/// clean and never reports the clear again.
#[test]
fn an_agent_clear_that_reaches_the_other_server_ends_the_taint() {
    let database = test_database();
    let url = database.url();
    Runtime::new().unwrap().block_on(async {
        let first = server(&url, "live_update_clear_first").await;
        let second = server(&url, "live_update_clear_second").await;
        let session = job_agent(&first, &job_hello("burst-1")).await;

        let mut tainted = session.beat();
        tainted["taint"] = residue("r1");
        let reply = post(&first, "/v1/agent/heartbeat", Some(JOIN), &tainted).await;
        assert_eq!(reply.status, 200, "{}", reply.body);
        assert_eq!(reply.json()["taint_ack"], true);

        let mut cleared = session.beat();
        cleared["taint_cleared"] = residue("r1");
        let reply = post(&second, "/v1/agent/heartbeat", Some(JOIN), &cleared).await;
        assert_eq!(reply.status, 200, "{}", reply.body);
        assert_eq!(reply.json()["taint_cleared_ack"], true);
        first.running.shutdown().await.unwrap();

        let third = server(&url, "live_update_clear_third").await;
        let mut host = Agent::new("burst-1");
        host.hello(&third).await;
        let listed = hosts(&third).await;
        assert_eq!(entry(&listed, "burst-1")["taint"], Value::Null, "{listed}");
        submit(&third, "job-1").await;
        third.ticks.step().await;
        host.beat(&third).await;
        assert_eq!(host.running.len(), 1, "the host runs the job");
    });
}
