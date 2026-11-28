//! What a server over a Postgres store holds as a steady load goes on: the bytes live in the
//! process (the binary's allocator counts them, `crate::ALLOC`) after a warm-up and after each
//! of two equal windows of rounds of the same work. A server that keeps something for each
//! request, heartbeat, statement or job and never lets it go holds more after the second window
//! than after the first by about what the first added; a bounded one holds about the same.
//!
//! The count is of the whole process, so it is the server's only where the test runs in a
//! process of its own, as nextest runs every test (the PR check and the coverage job). Nothing
//! waits on time: rounds and the server's ticks, stepped by hand, drive every step.

use std::collections::BTreeMap;
use std::net::SocketAddr;

use js_state::postgres::test_database;
use serde_json::Value;
use tokio::runtime::Runtime;

use crate::agents_support::{Session, job_hello};
use crate::jobs_support::{commands, exited, running};
use crate::live_update_rig::{HOSTS, post_at, server, submit_at};
use crate::support::Harness;

/// The rounds before the first count: the pool's connections, the runtime's threads and the
/// server's tables reach the size the load gives them.
const WARM: usize = 100;

/// The rounds of each window between two counts.
const WINDOW: usize = 150;

/// The most the bytes live may rise over the second window: what a bounded server's buffers
/// and caches move by between two quiet moments, far below what a window adds when something
/// is kept for each request (a window is about a thousand requests).
const SLACK: usize = 512 * 1024;

/// What the hosts do each round besides their heartbeat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Work {
    /// Nothing: the hosts beat with empty listings.
    Beats,
    /// A job is submitted each round; each host ends the job it has run longest.
    Jobs,
    /// As [`Work::Jobs`], and the first host lists, besides its own, a job the store holds no
    /// record of (an orphan), which it keeps running.
    JobsAndOrphan,
}

/// A fake per-job agent that keeps only what it runs and what it ended and was not answered.
struct Host {
    session: Option<Session>,
    name: String,
    running: BTreeMap<String, String>,
    ended: BTreeMap<String, String>,
    orphan: Option<String>,
}

impl Host {
    fn new(n: usize, orphan: bool) -> Self {
        let name = format!("burst-{n}");
        Self {
            session: None,
            orphan: orphan.then(|| format!("orphan-of-{name}")),
            name,
            running: BTreeMap::new(),
            ended: BTreeMap::new(),
        }
    }

    async fn hello(&mut self, server: SocketAddr) {
        let reply = post_at(server, "/v1/agent/hello", &job_hello(&self.name)).await;
        assert_eq!(reply.status, 200, "{}: {}", self.name, reply.body);
        let welcome = reply.json();
        self.session = Some(Session {
            host: self.name.clone(),
            epoch: welcome["epoch"].as_u64().unwrap(),
            secret: welcome["session"].as_str().unwrap().to_owned(),
        });
    }

    /// One heartbeat with the whole listing: starts are run, answered results let go.
    async fn beat(&mut self, server: SocketAddr) {
        let mut body = self.session.as_ref().unwrap().beat();
        let runs = self.running.keys().chain(self.orphan.iter()).map(|id| running(id));
        let ends = self.ended.iter().map(|(id, alloc)| exited(id, alloc));
        body["jobs"] = Value::Array(runs.chain(ends).collect());
        let reply = post_at(server, "/v1/agent/heartbeat", &body).await;
        assert_eq!(reply.status, 200, "{}: {}", self.name, reply.body);
        let reply = reply.json();
        let (starts, stops) = commands(&reply);
        assert_eq!(stops, Vec::<String>::new(), "{}: {reply}", self.name);
        for (job, allocation) in starts {
            if !self.ended.contains_key(&job) {
                self.running.entry(job).or_insert(allocation);
            }
        }
        for ack in reply["job_acks"].as_array().into_iter().flatten() {
            let job = ack["job_id"].as_str().unwrap_or_default();
            self.ended.remove(job);
        }
    }

    fn end_one(&mut self) {
        if let Some((job, allocation)) = self.running.pop_first() {
            self.ended.insert(job, allocation);
        }
    }
}

/// The bytes live in the process.
fn live() -> usize {
    crate::ALLOC.allocated()
}

/// Runs `work` on a server over a fresh Postgres database: [`WARM`] rounds, then two windows of
/// [`WINDOW`] rounds, counting the bytes live after each. The three counts.
fn soak(name: &str, work: Work) -> [usize; 3] {
    let database = test_database();
    let url = database.url();
    Runtime::new().unwrap().block_on(async {
        let h = server(&url, name).await;
        let jobs = h.running.job_addr().unwrap();
        let mut hosts: Vec<Host> = (1..=HOSTS)
            .map(|n| Host::new(n, n == 1 && work == Work::JobsAndOrphan))
            .collect();
        for host in &mut hosts {
            host.hello(h.addr()).await;
        }
        let mut next = 0u64;
        let mut counts = [0usize; 3];
        for round in 0..WARM + 2 * WINDOW {
            if let Some(at) = [WARM, WARM + WINDOW].iter().position(|at| *at == round) {
                counts[at] = quiet_count(&h).await;
            }
            if work != Work::Beats {
                next += 1;
                let made = submit_at(jobs, format!("soak-{next:05}")).await;
                assert_eq!(made.status, 201, "{}", made.body);
            }
            for host in &mut hosts {
                host.beat(h.addr()).await;
                host.end_one();
            }
            h.ticks.step().await;
        }
        counts[2] = quiet_count(&h).await;
        counts
    })
}

/// The bytes live once the server has finished two ticks after the round, so no pass of the
/// last round is still running.
async fn quiet_count(h: &Harness) -> usize {
    h.ticks.step().await;
    h.ticks.step().await;
    live()
}

/// Prints the counts and asserts that the second window added at most [`SLACK`].
fn bounded(work: Work, counts: [usize; 3]) {
    let [warm, first, second] = counts.map(|count| i64::try_from(count).unwrap());
    println!(
        "{work:?}: live after warm-up {warm} B, after the first window {first} B ({:+} B), after the second {second} B ({:+} B)",
        first - warm,
        second - first,
    );
    assert!(
        second - first <= i64::try_from(SLACK).unwrap(),
        "{work:?}: the second window of {WINDOW} rounds added {} B (the first {} B): the server keeps something for each round",
        second - first,
        first - warm,
    );
}

/// Asserts that hosts beating with empty listings, round after round, leave the server holding
/// no more after the second window than after the first. It catches a server that keeps
/// something for each heartbeat (a reading, a reply, a statement's result) that it never drops.
#[test]
fn heartbeats_alone_leave_the_bytes_live_level() {
    bounded(Work::Beats, soak("soak_beats", Work::Beats));
}

/// Asserts that a job submitted, placed, run and ended each round leaves the server holding no
/// more after the second window than after the first. It catches a server that keeps something
/// for each job, allocation, placement pass or store statement that it never drops.
#[test]
fn a_steady_job_cycle_leaves_the_bytes_live_level() {
    bounded(Work::Jobs, soak("soak_jobs", Work::Jobs));
}

/// Asserts the same of the job cycle with a host that lists an orphan on every beat. It catches
/// a server that keeps something for each beat that lists a job the store holds no record of.
#[test]
fn a_job_cycle_beside_an_orphan_leaves_the_bytes_live_level() {
    bounded(Work::JobsAndOrphan, soak("soak_orphan", Work::JobsAndOrphan));
}
