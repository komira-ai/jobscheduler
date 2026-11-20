//! The dispatch poll against a real lease server in the test process, with the job tables in
//! a SQLite state store the test reads too, and a real agent session, poller and worker on
//! threads of their own, over a container runtime in memory. The agent's heartbeat waits are
//! opened one at a time by the test; the poller's transports are wrapped so that the test
//! sees every poll and answer, and holds the poller on an answer that hands out a start until
//! the test has read the queue. Every wait is for an event with a hang guard.

use std::path::Path;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, PoisonError};

use js_agent::{Answer, Connector, Dialer, Login, Transport};
use js_agent_wire::{DISPATCH_PATH, DispatchKind as WireKind};
use js_client::ClientError;
use js_jobs::{DispatchKind, DispatchMessage, JobStore, RandomIds};
use js_types::HostId;
use serde_json::Value;

use super::GUARD;
use crate::jobs::drain::container_flags;
use crate::jobs::fake::FakeMachine;
use crate::server::{Agent, POOL_RUNS_QUICK_TOML, Server, Spy};
use crate::support::{Scratch, code, linux_host};

/// What the test sees of the poller's transports.
enum Event {
    /// A poll was sent, with its body.
    Polled(Value),
    /// A poll was answered 200, with the answer's body.
    Answered(Value),
}

/// What the poller's transports share with the test.
struct Shared {
    events: Mutex<Sender<Event>>,
    /// An answer that hands out a message is held until the test sends here.
    release: Mutex<Receiver<()>>,
}

/// The connector the poller makes its transports with: the real one, watched.
struct Watching(Arc<Shared>);

struct Watched {
    inner: Box<dyn Transport>,
    shared: Arc<Shared>,
}

impl Watched {
    fn tell(&self, event: Event) {
        let events = self
            .shared
            .events
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let _ = events.send(event);
    }
}

impl Connector for Watching {
    fn connect(
        &self,
        server: &str,
        login: &Login,
        ca_cert: Option<&Path>,
    ) -> Result<Box<dyn Transport>, ClientError> {
        let inner = Dialer.connect(server, login, ca_cert)?;
        Ok(Box::new(Watched {
            inner,
            shared: self.0.clone(),
        }))
    }
}

impl Transport for Watched {
    fn post(&mut self, path: &str, body: &Value) -> Result<Answer, String> {
        assert_eq!(path, DISPATCH_PATH);
        self.tell(Event::Polled(body.clone()));
        let answer = self.inner.post(path, body)?;
        if answer.status == 200 {
            self.tell(Event::Answered(answer.body.clone()));
            let handed = answer.body["messages"]
                .as_array()
                .is_some_and(|messages| !messages.is_empty());
            if handed {
                let release = self
                    .shared
                    .release
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                release.recv_timeout(GUARD).expect("the test let go");
            }
        }
        Ok(answer)
    }

    fn adopt_credential(&mut self, secret: &str) -> Result<(), String> {
        self.inner.adopt_credential(secret)
    }
}

/// The test's end of the poller's transports.
struct Poller {
    events: Receiver<Event>,
    release: Sender<()>,
}

impl Poller {
    /// The next event, in the order the poller made them.
    fn next(&self) -> Event {
        self.events.recv_timeout(GUARD).expect("the poller polled")
    }

    /// The next poll sent, and every answer that came before it.
    fn poll(&self, answers: &mut Vec<Value>) -> Value {
        loop {
            match self.next() {
                Event::Polled(body) => return body,
                Event::Answered(body) => answers.push(body),
            }
        }
    }

    /// The next answer that hands out a message (the poller is held on it), and every poll
    /// sent before it.
    fn handed(&self, polls: &mut Vec<Value>) -> Value {
        loop {
            match self.next() {
                Event::Polled(body) => polls.push(body),
                Event::Answered(body) if body["messages"] == serde_json::json!([]) => {}
                Event::Answered(body) => return body,
            }
        }
    }

    /// Lets the poller go on from the answer it is held on.
    fn go(&self) {
        self.release.send(()).expect("the poller waits");
    }
}

/// Beats the agent once: its wait passes, and the worker is done with the reply.
#[track_caller]
fn beat(agent: &Agent, worker: &js_agent::Handle) {
    agent.elapse();
    agent.next_wait();
    assert!(worker.settle());
}

/// Beats until `done`, at most `BEATS` times: a guard against a hang, not an expectation.
#[track_caller]
fn beats_until(agent: &Agent, worker: &js_agent::Handle, what: &str, done: impl Fn() -> bool) {
    const BEATS: usize = 20;
    for _ in 0..BEATS {
        if done() {
            return;
        }
        beat(agent, worker);
    }
    assert!(done(), "{what} after {BEATS} beats");
}

/// The message of the queue for `host`, as the test reads it from the server's own store.
fn queue(jobs: &JobStore<Arc<dyn js_state::Store>, RandomIds>, host: &str) -> Vec<DispatchMessage> {
    jobs.dispatch_queue(&HostId::new(host).unwrap()).unwrap()
}

/// The messages of an answer, as `(kind, job)`.
fn handed_out(answers: &[Value]) -> Vec<(WireKind, String)> {
    let messages = answers
        .iter()
        .flat_map(|a| a["messages"].as_array().unwrap());
    let kind = |m: &Value| serde_json::from_value(m["kind"].clone()).unwrap();
    messages
        .map(|m| (kind(m), m["job_id"].as_str().unwrap().to_owned()))
        .collect()
}

/// Asserts the whole path of a start through the dispatch route against a real server and a
/// real agent. The agent's hello reports dispatch-poll and the welcome says `dispatch_poll`;
/// two jobs are submitted through the job service, each placed on the host at once. The
/// first start arrives in an answer of `POST /v1/agent/dispatch` while its message in the
/// server's own queue is claimed by this server; the agent's next poll acknowledges it, so
/// that when the second start is handed out (which is after the server has applied that
/// acknowledgement) the first job is still unended and its message is gone, and the queue
/// holds the second's, claimed. A heartbeat between the hand-out and the agent's taking of the
/// start gets a reply without it. Both jobs run on the agent's fake runtime (created from the
/// route's starts alone), their containers exit, the server records both as succeeded with
/// the reason exit, the host's queue is empty, the two starts were the only messages the
/// route handed out, and no heartbeat reply carried a start or a stop. Catches a placed job
/// that never reaches a polling agent because the heartbeat's reply leaves it out and the
/// route does not hand it out, a route that hands a start out without claiming its message,
/// and an acknowledgement that does not delete it.
#[test]
fn a_placed_job_reaches_a_polling_agent_through_the_route_and_runs_to_its_end() {
    let server = Server::start_stored("jobs_dispatch_live", POOL_RUNS_QUICK_TOML);
    let jobs = JobStore::new(Arc::new(server.store()), RandomIds).unwrap();

    let files = Scratch::new("jobs_dispatch_live-files");
    let mut flags = container_flags(&files);
    flags.extend(["--host-id", "h1", "--drain-wait", "0"].map(str::to_owned));
    let flags: Vec<&str> = flags.iter().map(String::as_str).collect();
    let (events, seen) = channel();
    let (release, held) = channel();
    let shared = Arc::new(Shared {
        events: Mutex::new(events),
        release: Mutex::new(held),
    });
    let poller = Poller {
        events: seen,
        release,
    };
    let heartbeats = Spy::default();
    let answered = heartbeats.0.clone();
    let machine = Arc::new(FakeMachine::new());
    let agent = Agent::spawn_polling(
        server.url(),
        server.join_file(),
        &flags,
        linux_host,
        (heartbeats, Arc::new(Watching(shared))),
        machine.clone(),
    );
    let worker = agent.worker();
    agent.next_wait();

    // The welcome says the host polls; the first poll names the host.
    let welcome = answered.lock().unwrap()[0].clone();
    assert_eq!(welcome["dispatch_poll"], true, "{welcome}");
    let (mut polls, mut answers) = (Vec::new(), Vec::new());
    let first = poller.poll(&mut answers);
    assert_eq!(first["host_id"], "h1");
    assert!(first.get("acks").is_none(), "{first}");
    assert!(queue(&jobs, "h1").is_empty());

    // The first job is placed at once and its start is handed out by the route, claimed.
    let a = submit(&server, 1000);
    assert_eq!(server.job_state(&a), "JOB_STATE_PLACED");
    let handing = poller.handed(&mut polls);
    assert_eq!(
        handed_out(std::slice::from_ref(&handing)),
        [(WireKind::Start, a.clone())]
    );
    let held = queue(&jobs, "h1");
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].kind, DispatchKind::Start);
    assert_eq!(held[0].job.to_string(), a);
    assert!(held[0].claimed_by.is_some(), "{:?}", held[0]);
    let allocation = handing["messages"][0]["allocation_id"].clone();
    answers.push(handing);

    // A heartbeat while the poller holds the start, which the agent has not taken yet: its
    // reply leaves the start out, so nothing is created from it.
    beat(&agent, &worker);
    assert!(machine.world.created().is_empty());
    poller.go();

    // The agent acknowledges the start in its next poll.
    let acknowledging = poller.poll(&mut answers);
    assert_eq!(
        acknowledging["acks"],
        serde_json::json!([{ "kind": "start", "allocation_id": allocation }])
    );
    assert_eq!(server.job_state(&a), "JOB_STATE_PLACED");

    // The second start's answer comes after the server applied that acknowledgement: the
    // first job is unended and its message is gone, the second's is claimed.
    let b = submit(&server, 1000);
    let handing = poller.handed(&mut polls);
    assert_eq!(
        handed_out(std::slice::from_ref(&handing)),
        [(WireKind::Start, b.clone())]
    );
    let held = queue(&jobs, "h1");
    assert_eq!(held.len(), 1, "{held:?}");
    assert_eq!(held[0].job.to_string(), b);
    assert!(held[0].claimed_by.is_some(), "{:?}", held[0]);
    assert_eq!(server.job_state(&a), "JOB_STATE_PLACED");
    let allocation = handing["messages"][0]["allocation_id"].clone();
    answers.push(handing);
    poller.go();
    let acknowledging = poller.poll(&mut answers);
    assert_eq!(
        acknowledging["acks"],
        serde_json::json!([{ "kind": "start", "allocation_id": allocation }])
    );

    // The worker has both starts: the containers exist, from the route's starts alone.
    assert!(worker.settle());
    assert_eq!(machine.world.created(), [a.as_str(), b.as_str()]);
    beats_until(&agent, &worker, "both jobs are not running", || {
        [&a, &b]
            .iter()
            .all(|job| server.job_state(job) == "JOB_STATE_RUNNING")
    });

    machine.world.exit(&a, 0);
    machine.world.exit(&b, 0);
    assert!(worker.settle());
    beats_until(&agent, &worker, "both jobs have not ended", || {
        [&a, &b]
            .iter()
            .all(|job| server.job_state(job) == "JOB_STATE_SUCCEEDED")
    });
    for job in [&a, &b] {
        let result = server.job_result(job);
        assert_eq!(result.body["result"]["reason"], "RESULT_REASON_EXIT");
    }
    assert!(queue(&jobs, "h1").is_empty());

    // Only the route handed out the starts, and a heartbeat reply carried no start or stop.
    assert_eq!(
        handed_out(&answers),
        [(WireKind::Start, a.clone()), (WireKind::Start, b.clone())]
    );
    let replies = answered.lock().unwrap().clone();
    let carried: Vec<&Value> = replies
        .iter()
        .flat_map(|reply| reply["commands"].as_array().into_iter().flatten())
        .collect();
    assert_eq!(carried, Vec::<&Value>::new());

    drop(poller);
    let ran = agent.stop();
    assert_eq!(ran.status(), code(0), "{}", ran.err);
}

/// Submits a job that needs `cpu` thousandths of a core; returns its id.
fn submit(server: &Server, cpu: u64) -> String {
    let reply = server.submit(cpu);
    assert_eq!(reply.status, 201, "{}", reply.body);
    reply.body["job"]["id"].as_str().unwrap().to_owned()
}
