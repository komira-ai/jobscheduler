//! A rolling update of js-server while jobs run and more are submitted: the deployment's
//! two replicas updated one at a time, start-first, over one Postgres store. Four fake
//! per-job agents (three that poll dispatch, one of a release before the queue that takes its
//! starts from its heartbeat's reply) send every request to whichever server the edge picks;
//! jobs are submitted to every server that serves, and the hosts end what they run as rounds
//! go by. Each server is stopped as a termination signal stops it ([`signal`], then
//! `Running::shutdown`) while it is inside a transaction of a pull: a placement (its mint of the
//! allocation id parked by a [`Gate`]; the placement is its poller's, made after the submit was
//! answered queued) for the first roll, a dispatch claim (waiting on the
//! store's commit head, held by a [`HeadLock`]) for the second. A second test kills a server
//! after its claim of a dispatch message committed and before the agent read the reply.
//! Nothing waits on time: rounds, ticks and the barriers' events drive every step, and the
//! only clock moved is a server's [`crate::support::Harness`] clock, by hand.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use js_jobs::{JobStore, RandomIds};
use js_state::postgres::test_database;
use js_state::{ManualClock, StoreTime};
use js_types::HostId;
use tokio::runtime::Runtime;

use crate::jobs_support::{JobBody, left_the_queue, submit_body};
use crate::live_update_rig::{
    Agent, Busy, Edge, Gate, HOSTS, HeadLock, PER_HOST, Serving, due, post_at, server,
    server_minting, server_reading, signal, state_at, submit_at,
};
use crate::support::{Harness, T0, guard, request};

/// The hosts, and the jobs submitted so far, in order.
struct Fleet {
    agents: Vec<Agent>,
    jobs: Vec<String>,
    next: u32,
}

impl Fleet {
    /// [`HOSTS`] hosts; the last one does not poll dispatch.
    fn new() -> Self {
        let agents = (1..=HOSTS)
            .map(|n| Agent::new(&format!("burst-{n}"), n < HOSTS))
            .collect();
        Self {
            agents,
            jobs: Vec::new(),
            next: 0,
        }
    }

    /// The id of the next job.
    fn id(&mut self) -> String {
        self.next += 1;
        format!("job-{:03}", self.next)
    }

    /// Submits the next job to the job service at `job`, which must admit it.
    async fn submit(&mut self, job: SocketAddr) -> String {
        let id = self.id();
        let made = submit_at(job, id.clone()).await;
        assert_eq!(made.status, 201, "{id}: {}", made.body);
        self.jobs.push(id.clone());
        id
    }

    fn agent(&mut self, host: &str) -> &mut Agent {
        self.agents
            .iter_mut()
            .find(|agent| agent.host == host)
            .unwrap()
    }

    /// How many jobs the hosts hold (run, or ended with the end not acknowledged).
    fn held(&self) -> usize {
        self.agents.iter().map(Agent::holds).sum()
    }

    /// The jobs no host was told to start.
    fn unstarted(&self) -> Vec<String> {
        let told = |id: &String| self.agents.iter().any(|a| a.starts.contains_key(id));
        self.jobs.iter().filter(|id| !told(id)).cloned().collect()
    }

    /// One round: each host beats, a polling host to which a start is due (as the job
    /// service at `reader` says) polls, each host ends a job when `end`, and each of
    /// `servers` ticks.
    async fn round(
        &mut self,
        edge: &mut Edge,
        reader: SocketAddr,
        end: bool,
        servers: &[&Harness],
    ) {
        let unstarted = self.unstarted();
        for agent in self.agents.iter_mut().filter(|agent| agent.joined()) {
            agent.beat(edge.pick()).await;
            if agent.polls() && due(reader, &unstarted, agent).await {
                agent.poll(edge.pick()).await;
            }
            if end {
                agent.end_one();
            }
        }
        for server in servers {
            server.ticks.step().await;
        }
    }

    /// Rounds until every job was started and the hosts hold at most `most` for two rounds in
    /// a row, so the next job submitted is placed at once by its server's poller and is the
    /// only one placed. The two rounds are for the servers: a host's container of an
    /// acknowledged end is freed by its later listings, so a server counts it until then.
    async fn calm(
        &mut self,
        edge: &mut Edge,
        reader: SocketAddr,
        most: usize,
        servers: &[&Harness],
    ) {
        let mut quiet = 0;
        for _ in 0..20 {
            if self.unstarted().is_empty() && self.held() <= most {
                quiet += 1;
                if quiet > 2 {
                    return;
                }
            } else {
                quiet = 0;
            }
            let end = self.held() > most;
            self.round(edge, reader, end, servers).await;
        }
        panic!("the fleet did not calm: {:?} unstarted", self.unstarted());
    }
}

/// Asserts that a rolling update of two servers under load fails no job. Server A serves the
/// three hosts that poll dispatch and runs jobs; server B (the next release) starts beside it
/// and both serve, jobs submitted to each. A job is answered queued when submitted; its
/// server's poller places it a moment later. A is signalled while a placement its poller
/// pulled is parked inside its claimed transaction: the placement commits and its start
/// reaches its host (one that polls) from B at B's first claim, so A left no claim to lapse;
/// a job submitted to A after the signal is admitted and stays queued through A's tick,
/// though the hosts have room, until B places it. The fourth host, of the release before the queue, says hello to B
/// then.
/// A exits; server C (the release after) starts beside B, and B is signalled while its claim
/// of a dispatch message waits inside its transaction: the claim commits and the poll is
/// answered with the start, and a job submitted to B after the signal stays queued through B's
/// tick. B exits and C serves alone until every host has ended what it runs. In the end every
/// job submitted succeeded, each was started under one allocation on one host and that start
/// reached its host once, each agent said hello once (every later beat and poll, on any
/// server, was answered 200: no `unknown-host`, `session-mismatch` or `host-live`), and no
/// agent was told to stop a job.
///
/// It catches a server that refuses a beat or a poll of a session another server's hello
/// began (the agent would say hello again), a draining server that places after the signal or
/// leaves a claim behind, a pulled placement or claim the drain cuts or rolls back (its job
/// would be lost or started twice), a job submitted during the overlap refused or left queued,
/// a start handed out twice, and a job lost, failed or doubled across either roll.
#[test]
fn a_rolling_update_of_two_servers_under_load_fails_no_job() {
    let database = test_database();
    let url = database.url();
    Runtime::new().unwrap().block_on(async {
        let gate = Arc::new(Gate::default());
        let a = server_minting(&url, "load_a", Arc::clone(&gate) as _).await;
        let a_jobs = a.running.job_addr().unwrap();
        let mut edge = Edge::default();
        edge.add(&a);
        let mut fleet = Fleet::new();
        // The host of the release before the queue says hello during the first overlap.
        for agent in fleet.agents.iter_mut().filter(|agent| agent.polls()) {
            agent.hello(a.addr()).await;
        }
        for _ in 0..3 {
            fleet.submit(a_jobs).await;
            fleet.submit(a_jobs).await;
            fleet.round(&mut edge, a_jobs, false, &[&a]).await;
        }

        // The first roll: B starts beside A, and both serve and take jobs.
        let b = server(&url, "load_b").await;
        let b_jobs = b.running.job_addr().unwrap();
        b.ticks.step().await;
        edge.add(&b);
        for n in 0..4 {
            fleet.submit(a_jobs).await;
            fleet.submit(b_jobs).await;
            fleet.round(&mut edge, b_jobs, n % 2 == 1, &[&a, &b]).await;
        }

        // A is signalled while a placement it pulled is parked inside its transaction: the
        // job's id is the client's, so the first mint is the placement's of the allocation,
        // made by A's poller after the submit was answered queued.
        // Room for the pulled job and one more on the three hosts that poll.
        fleet
            .calm(&mut edge, b_jobs, (HOSTS - 1) * PER_HOST - 2, &[&a, &b])
            .await;
        gate.arm(0);
        let pulled = fleet.id();
        let made = submit_at(a_jobs, pulled.clone()).await;
        assert_eq!(made.status, 201, "{}", made.body);
        assert_eq!(
            made.json()["job"]["state"],
            "JOB_STATE_QUEUED",
            "{}",
            made.body
        );
        fleet.jobs.push(pulled.clone());
        let parked = gate.parked().await;
        signal(&a).await;
        edge.remove(&a);
        parked.release();
        let placed = left_the_queue(&b, &pulled).await;
        assert_eq!(placed["state"], "JOB_STATE_PLACED", "{placed}");
        // A claimed nothing after the signal: B hands the start out at its first claim.
        let host = placed["hostId"].as_str().unwrap().to_owned();
        let agent = fleet.agent(&host);
        assert!(agent.polls(), "{host}");
        agent.poll(b.addr()).await;
        assert!(agent.starts.contains_key(&pulled), "{host}: {pulled}");
        // A places nothing after the signal, though the hosts have room; a job submitted to
        // either server now is admitted.
        let late = fleet.submit(a_jobs).await;
        a.ticks.step().await;
        assert_eq!(state_at(a_jobs, &late).await, "JOB_STATE_QUEUED");
        fleet.submit(b_jobs).await;
        let joining = HOSTS - 1;
        fleet.agents[joining].hello(edge.pick()).await;
        for n in 0..3 {
            fleet.round(&mut edge, b_jobs, n % 2 == 1, &[&a, &b]).await;
        }
        a.running.shutdown().await.unwrap();
        let placed = [
            "JOB_STATE_PLACED",
            "JOB_STATE_RUNNING",
            "JOB_STATE_SUCCEEDED",
        ];
        let moved = state_at(b_jobs, &late).await;
        assert!(placed.contains(&moved.as_str()), "{late}: {moved}");

        // The second roll: C starts beside B, and both serve and take jobs.
        let c = server(&url, "load_c").await;
        let c_jobs = c.running.job_addr().unwrap();
        c.ticks.step().await;
        edge.add(&c);
        for n in 0..4 {
            fleet.submit(b_jobs).await;
            fleet.submit(c_jobs).await;
            fleet.round(&mut edge, c_jobs, n % 2 == 0, &[&b, &c]).await;
        }

        // B is signalled while its claim of a dispatch message waits inside its transaction.
        // The job is one B placed on a host that polls (a job placed on the host that does not
        // is delivered by a round, and the next is submitted).
        let mut claimed = None;
        for _ in 0..HOSTS * PER_HOST {
            fleet
                .calm(&mut edge, c_jobs, HOSTS * PER_HOST - 2, &[&b, &c])
                .await;
            let id = fleet.submit(b_jobs).await;
            let read = left_the_queue(&b, &id).await;
            assert_eq!(read["state"], "JOB_STATE_PLACED", "{read}");
            let host = read["hostId"].as_str().unwrap().to_owned();
            if fleet.agent(&host).polls() {
                claimed = Some((id, host));
                break;
            }
        }
        let (claimed, host) = claimed.expect("a job placed on a host that polls");
        let head = HeadLock::take(&url).await;
        let body = fleet.agent(&host).poll_body(false);
        let b_agents = b.addr();
        let polling =
            tokio::spawn(async move { post_at(b_agents, "/v1/agent/dispatch", &body).await });
        head.waited_on().await;
        signal(&b).await;
        edge.remove(&b);
        head.release().await;
        let reply = guard(polling).await.unwrap();
        let agent = fleet.agent(&host);
        agent.answered(&reply);
        assert!(
            agent.starts.contains_key(&claimed),
            "{host}: {}",
            reply.body
        );
        let late = fleet.submit(b_jobs).await;
        b.ticks.step().await;
        assert_eq!(state_at(b_jobs, &late).await, "JOB_STATE_QUEUED");
        fleet.submit(c_jobs).await;
        for n in 0..3 {
            fleet.round(&mut edge, c_jobs, n % 2 == 1, &[&b, &c]).await;
        }
        b.running.shutdown().await.unwrap();

        // C serves alone until every host has ended what it runs.
        for _ in 0..40 {
            if fleet.unstarted().is_empty() && fleet.held() == 0 {
                break;
            }
            fleet.round(&mut edge, c_jobs, true, &[&c]).await;
        }
        let mut ended = BTreeMap::new();
        for id in &fleet.jobs {
            ended.insert(id.clone(), state_at(c_jobs, id).await);
        }
        let failed: Vec<_> = ended
            .iter()
            .filter(|(_, state)| *state != "JOB_STATE_SUCCEEDED")
            .collect();
        assert_eq!(
            failed,
            Vec::<(&String, &String)>::new(),
            "every job succeeded"
        );

        let mut started: BTreeMap<&str, Vec<(&str, &str)>> = BTreeMap::new();
        for agent in &fleet.agents {
            assert_eq!(agent.hellos, 1, "{}: one hello in all", agent.host);
            for (job, allocations) in &agent.starts {
                for allocation in allocations {
                    let on = started.entry(job).or_default();
                    on.push((agent.host.as_str(), allocation.as_str()));
                }
            }
        }
        assert_eq!(started.len(), fleet.jobs.len(), "{started:?}");
        for (job, on) in &started {
            assert_eq!(on.len(), 1, "{job}: one allocation on one host: {on:?}");
        }
        for agent in &fleet.agents {
            for (start, times) in &agent.deliveries {
                assert_eq!(*times, 1, "{}: {start:?} reached it once", agent.host);
            }
        }
        c.running.shutdown().await.unwrap();
    });
}

/// Asserts that a server killed mid-dispatch leaves the start to the other server once the
/// claim's lease is out, and the job starts once. A host says hello to server A, which places
/// a job on it; the host's poll reaches A, whose claim of the start commits, and A dies before
/// the agent reads the reply (the reply is lost with it): no drain, no final tick. The host's
/// next beat reaches server B and is served (no hello again), and the job is still placed: not
/// lost, not failed to start. Once B's clock is past the claim's lease (the store's time B
/// reads, `server.dispatch_lease_secs`, 10 s by default), the host's poll to B is handed the
/// same start, under the allocation A claimed; the host runs the job once and it succeeds. It
/// catches a start a dead server's claim strands (never handed out again), one handed out
/// under another allocation (a second placement), and a job the kill ends.
#[test]
fn a_server_killed_mid_dispatch_leaves_the_start_to_the_other_after_the_lease() {
    let database = test_database();
    let url = database.url();
    Runtime::new().unwrap().block_on(async {
        let a = server(&url, "crash_a").await;
        let b = server(&url, "crash_b").await;
        let b_jobs = b.running.job_addr().unwrap();
        let mut agent = Agent::new("burst-1", true);
        agent.hello(a.addr()).await;
        let made = submit_at(a.running.job_addr().unwrap(), "job-1".to_owned()).await;
        assert_eq!(made.status, 201, "{}", made.body);
        assert_eq!(
            made.json()["job"]["state"],
            "JOB_STATE_QUEUED",
            "{}",
            made.body
        );
        let lost = post_at(a.addr(), "/v1/agent/dispatch", &agent.poll_body(false)).await;
        assert_eq!(lost.status, 200, "{}", lost.body);
        let messages = lost.json()["messages"].clone();
        assert_eq!(messages[0]["job_id"], "job-1", "{messages}");
        let allocation = messages[0]["allocation_id"].as_str().unwrap().to_owned();
        // Killed: the reply never reached the agent.
        drop(a);

        agent.beat(b.addr()).await;
        assert_eq!(state_at(b_jobs, "job-1").await, "JOB_STATE_PLACED");
        // The start is still A's in the store: claimed until the lease ends, so no server
        // hands it out while the store's clock reads at or before then.
        let lease = Duration::from_secs(10);
        let claimed = StoreTime::from_duration(Duration::from_millis(T0)).saturating_add(lease);
        let store = b.running.store().expect("B has a store");
        let jobs = JobStore::new(Arc::new(store), RandomIds).unwrap();
        let queue = jobs
            .dispatch_queue(&HostId::new("burst-1").unwrap())
            .unwrap();
        assert_eq!(queue.len(), 1, "{queue:?}");
        assert!(queue[0].claimed_by.is_some(), "{queue:?}");
        assert_eq!(queue[0].claim_until, Some(claimed), "{queue:?}");
        assert!(!queue[0].is_due(claimed), "{queue:?}");
        b.clock.advance(lease + Duration::from_millis(1));
        agent.poll(b.addr()).await;
        let starts: Vec<&String> = agent.starts["job-1"].iter().collect();
        assert_eq!(starts, vec![&allocation], "the start A claimed");
        let once = BTreeMap::from([(("job-1".to_owned(), allocation.clone()), 1)]);
        assert_eq!(agent.deliveries, once, "the start reached the agent once");
        agent.beat(b.addr()).await;
        assert_eq!(state_at(b_jobs, "job-1").await, "JOB_STATE_RUNNING");
        agent.end_all();
        agent.beat(b.addr()).await;
        b.ticks.step().await;
        assert_eq!(state_at(b_jobs, "job-1").await, "JOB_STATE_SUCCEEDED");
        assert!(agent.done.contains("job-1"));
        assert_eq!(agent.hellos, 1);
        b.running.shutdown().await.unwrap();
    });
}

/// Asserts that a server starts beside one that changes a job before every call the new
/// server's job store open makes (a writer that never pauses, bounded by a count, writing
/// through the serving server's own job store): the new server's start returns and its
/// readiness route answers 200 `ready` with changes still left to the writer, so it did not
/// need the serving server to stop; every change of the serving server committed at its first
/// try; and the roll goes on: the serving server is signalled and exits, and a job submitted
/// to the new server is admitted and read back. It catches an open that starts over each time
/// a job changes under it (the new server never turns ready on a busy farm, and the update
/// never finishes), and an open that makes the serving server lose a write.
#[test]
fn a_server_turns_ready_while_the_serving_server_changes_jobs_without_a_pause() {
    /// The jobs the serving server holds when the new server starts.
    const JOBS: usize = 40;
    /// The most changes the serving server makes: far more than the open needs calls.
    const STEPS: usize = 600;
    let database = test_database();
    let url = database.url();
    Runtime::new().unwrap().block_on(async {
        let a = server(&url, "busy_a").await;
        let a_jobs = a.running.job_addr().unwrap();
        // A host joined, so the jobs are admitted. A job that takes all of the host's 8 cores
        // is placed on it first, so no seed has room there and every seed stays queued: A's
        // placement poller, which each submit pokes and which runs on a task of its own, places
        // none of them, whenever its pass runs, under the serving server's writes.
        Agent::new("burst-1", true).hello(a.addr()).await;
        let made = submit_body(&a, &JobBody::new("filler").needs(8000, 1 << 30).json()).await;
        assert_eq!(made.status, 201, "{}", made.body);
        let placed = left_the_queue(&a, "filler").await;
        assert_eq!(placed["state"], "JOB_STATE_PLACED", "{placed}");
        let mut ids = Vec::new();
        for n in 0..JOBS {
            let id = format!("seed-{n:03}");
            let made = submit_at(a_jobs, id.clone()).await;
            assert_eq!(made.status, 201, "{id}: {}", made.body);
            ids.push(id);
        }
        let store = a.running.store().expect("A has a store");
        let serving = Serving::new(store, &ids, STEPS);
        let clock = ManualClock::default();
        // An hour on from the jobs' times, so the open has deadlines to raise.
        clock.advance(Duration::from_millis(T0) + Duration::from_secs(3600));
        let busy = Arc::new(Busy::new(clock, serving));

        // The first roll: B starts while the serving server changes a job at every call.
        let b = server_reading(&url, "busy_b", busy.clone()).await;
        let (left, made) = busy.stop();
        assert!(
            left > 0,
            "the open needed the serving server to stop: all {made} changes made"
        );
        let ready = b
            .running
            .ready_addr()
            .expect("B serves its readiness route");
        let reply = request(ready, "GET", "/readyz", None, None).await;
        assert_eq!((reply.status, reply.body.as_str()), (200, "ready"));

        // A is signalled and exits; a job submitted to B is admitted.
        signal(&a).await;
        a.running.shutdown().await.unwrap();
        let b_jobs = b.running.job_addr().unwrap();
        let made = submit_at(b_jobs, "after-roll".to_owned()).await;
        assert_eq!(made.status, 201, "{}", made.body);
        assert_eq!(state_at(b_jobs, "after-roll").await, "JOB_STATE_QUEUED");
        b.running.shutdown().await.unwrap();
    });
}
