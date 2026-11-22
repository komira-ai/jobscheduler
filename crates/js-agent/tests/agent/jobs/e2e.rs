//! A real server in the test process and a real agent session and worker on a thread
//! of their own, over a container runtime in memory and a clock the test moves. The agent's
//! waits are opened one at a time by the test, so each test reads as the order of events
//! it asserts: nothing here waits for a time to pass.
//!
//! The agent polls the dispatch route (its hello reports `dispatch-poll` and the real server
//! welcomes it with `dispatch_poll`), so the server leaves each start and stop out of a
//! heartbeat's reply and hands it out in an answer of the poll. The poller runs on a thread of
//! its own; its transports tell the rig when a poll is sent and when an answer handed out a
//! message ([`Rig::delivered`]), which is how a test waits for a start or a stop to reach the
//! worker: by event, as the heartbeats are.
//!
//! The rigs use a pool with the shortest heartbeat interval (250 ms, [`POOL_RUNS_QUICK_TOML`]):
//! the server holds the reply of a heartbeat that has nothing to send for up to that long, to
//! an agent that reports `held-beat`, and says `beat_now`, after which the agent beats at once
//! ([`HELD`]); so does a reply with acknowledgements and no command; a reply with a command
//! (a start or a stop of a run, a clear) is followed by the interval ([`INTERVAL`]). With
//! the default 5 s each quiet beat would hold the test for 5 s of real time.
//!
//! The jobs here are submitted through the v1 job API (`POST /v1/jobs` on `--job-listen`) and
//! placed by `js-sched` on the agent's host. The drain that lets a running job finish is also
//! asserted with the server's own binary and the crun runtime, in js-lease-server's
//! `v1_acceptance` target.

use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use js_agent::{Answer, Connector, Handle, Login, Transport};
use js_client::{ClientError, Reply};
use serde_json::{Value, json};

use super::dispatch::GUARD;
use super::fake::FakeMachine;
use super::{private, user};
use crate::server::{Agent, Counting, POOL_RUNS_QUICK_TOML, Recorder, Redirect, Server};
use crate::support::{Ran, Scratch, code, linux_host};

/// The heartbeat interval of the pool the rigs use ([`POOL_RUNS_QUICK_TOML`]).
const INTERVAL: Duration = Duration::from_millis(250);

/// The wait after a reply that says `beat_now`: the agent beats again at once. The server says
/// it for a reply it held (nothing to send) and for one that carries acknowledgements and no
/// command. A reply with a command (a start, a stop) is followed by [`INTERVAL`].
const HELD: Duration = Duration::ZERO;

/// The most beats a test waits for a job to be placed.
const BEATS: usize = 5;

/// The state of a job that is running, as the job service names it.
const RUNNING: &str = "JOB_STATE_RUNNING";

/// What the poller's transports tell the rig, in the order they happen.
enum Poll {
    /// A poll was sent.
    Sent,
    /// A poll was answered 200 with at least one message.
    Handed,
}

/// The connector of the poller's transports: the redirecting one, watched.
struct Announcing {
    inner: Redirect,
    events: Mutex<Sender<Poll>>,
}

struct Announced {
    inner: Box<dyn Transport>,
    events: Sender<Poll>,
}

impl Connector for Announcing {
    fn connect(
        &self,
        server: &str,
        login: &Login,
        ca_cert: Option<&Path>,
    ) -> Result<Box<dyn Transport>, ClientError> {
        let events = self.events.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(Box::new(Announced {
            inner: self.inner.connect(server, login, ca_cert)?,
            events: events.clone(),
        }))
    }
}

impl Transport for Announced {
    fn post(&mut self, path: &str, body: &Value) -> Result<Answer, String> {
        let _ = self.events.send(Poll::Sent);
        let answer = self.inner.post(path, body)?;
        let handed = answer.body["messages"]
            .as_array()
            .is_some_and(|messages| !messages.is_empty());
        if answer.status == 200 && handed {
            let _ = self.events.send(Poll::Handed);
        }
        Ok(answer)
    }

    fn adopt_credential(&mut self, secret: &str) -> Result<(), String> {
        self.inner.adopt_credential(secret)
    }
}

/// A server with one agent that runs containers.
struct Rig {
    polls: Receiver<Poll>,
    server: Server,
    agent: Agent,
    worker: Handle,
    machine: Arc<FakeMachine>,
    address: Arc<Mutex<String>>,
    /// The agent's flags, which the next process of the agent is started with too.
    flags: Vec<String>,
    files: Scratch,
}

/// Starts the server and the agent (`extra` flags beside the container ones), waits until
/// the agent has said hello.
fn rig(name: &str, extra: &[&str]) -> Rig {
    let server = Server::start_pool(name, &[], POOL_RUNS_QUICK_TOML);
    let address = Arc::new(Mutex::new(server.url()));
    rig_over(server, address, name, extra, Redirect)
}

fn rig_over<C: js_agent::Connector + Send + 'static>(
    server: Server,
    address: Arc<Mutex<String>>,
    name: &str,
    extra: &[&str],
    connector: impl Fn(Arc<Mutex<String>>) -> C,
) -> Rig {
    let files = Scratch::new(&format!("{name}-files"));
    rig_in(server, address, files, extra, connector)
}

/// [`rig_over`] with the agent's files in `files`, which the test may have put things in.
fn rig_in<C: js_agent::Connector + Send + 'static>(
    server: Server,
    address: Arc<Mutex<String>>,
    files: Scratch,
    extra: &[&str],
    connector: impl Fn(Arc<Mutex<String>>) -> C,
) -> Rig {
    let exec = files.write("docker", "#!/bin/sh\n", 0o755);
    let (user, _, _) = user();
    let state = private(&files, "state").display().to_string();
    let scratch = private(&files, "run-scratch").display().to_string();
    let exec = exec.display().to_string();
    let mut flags: Vec<String> = [
        "--host-id",
        "h1",
        "--allow-container-engine",
        "--container-bin",
        &exec,
        "--container-user",
        &user,
        "--container-subuid",
        "jobmap",
        "--state-root",
        &state,
        "--scratch-root",
        &scratch,
    ]
    .iter()
    .map(|flag| (*flag).to_owned())
    .collect();
    flags.extend(extra.iter().map(|flag| (*flag).to_owned()));
    let machine = Arc::new(FakeMachine::new());
    let (agent, worker, polls) = launch(&server, &address, &flags, &machine, connector);
    Rig {
        polls,
        server,
        agent,
        worker,
        machine,
        address,
        flags,
        files,
    }
}

/// Starts an agent of the rig's host with `flags` over `machine`, and waits until it has said
/// hello.
fn launch<C: js_agent::Connector + Send + 'static>(
    server: &Server,
    address: &Arc<Mutex<String>>,
    flags: &[String],
    machine: &Arc<FakeMachine>,
    connector: impl Fn(Arc<Mutex<String>>) -> C,
) -> (Agent, Handle, Receiver<Poll>) {
    let (events, polls) = channel();
    let dispatch = Announcing {
        inner: Redirect(address.clone()),
        events: Mutex::new(events),
    };
    let flags: Vec<&str> = flags.iter().map(String::as_str).collect();
    let agent = Agent::spawn_polling(
        server.url(),
        server.join_file(),
        &flags,
        linux_host,
        (connector(address.clone()), Arc::new(dispatch)),
        machine.clone(),
    );
    let worker = agent.worker();
    assert_eq!(agent.next_wait(), INTERVAL, "the hello");
    (agent, worker, polls)
}

impl Rig {
    /// Waits until the poller was handed a message (a start or a stop) and has given it to
    /// the worker (the poll after the answer is sent only then), and the worker is done with
    /// it. The agent is not asked to beat: nothing here depends on a heartbeat.
    #[track_caller]
    fn delivered(&self) {
        let next = |wanted: fn(&Poll) -> bool| loop {
            let event = self.polls.recv_timeout(GUARD);
            if wanted(&event.expect("the poller polled")) {
                break;
            }
        };
        next(|event| matches!(event, Poll::Handed));
        next(|event| matches!(event, Poll::Sent));
        assert!(self.worker.settle());
    }

    /// One heartbeat of the agent, and the worker done with the reply to it. `wait` is what the
    /// agent asks for after the reply: [`HELD`] when the server held the reply open for the
    /// beat, [`INTERVAL`] when the reply carried something.
    #[track_caller]
    fn beat(&self, wait: Duration) {
        self.agent.elapse();
        assert_eq!(self.agent.next_wait(), wait);
        assert!(self.worker.settle());
    }

    /// Submits a job that needs `cpu` thousandths of a core.
    fn queue(&self, cpu: u64) -> Reply {
        self.server.submit(cpu)
    }

    /// Submits a job that is expected to be accepted; returns its id.
    fn queued(&self, cpu: u64) -> String {
        let reply = self.queue(cpu);
        assert_eq!(reply.status, 201, "{}", reply.body);
        reply.body["job"]["id"].as_str().unwrap().to_owned()
    }

    /// What the server says of `job`: its state, and its result's reason when it has ended.
    fn view(&self, job: &str) -> (String, Option<String>) {
        let state = self.server.job_state(job);
        let result = self.server.job_result(job);
        let reason = result.body["result"]["reason"].as_str().map(str::to_owned);
        (state, reason)
    }

    /// One heartbeat of the agent, with no claim about the wait after it: the server places
    /// the jobs that wait on a heartbeat, and a placement's start leaves in the dispatch route.
    #[track_caller]
    fn tick(&self) {
        self.agent.elapse();
        self.agent.next_wait();
        assert!(self.worker.settle());
    }

    /// Beats until the server has placed `job`, at most [`BEATS`] times: a guard against a
    /// hang, not an expectation.
    #[track_caller]
    fn placed(&self, job: &str) {
        for _ in 0..BEATS {
            if self.server.job_state(job) != "JOB_STATE_QUEUED" {
                return;
            }
            self.tick();
        }
        assert_ne!(
            self.server.job_state(job),
            "JOB_STATE_QUEUED",
            "{BEATS} beats"
        );
    }

    /// Until the server lists `job` as running: it is placed (on a beat if it waited), its
    /// start arrives by the dispatch route, then the beat that lists it created and the one
    /// after, both held.
    #[track_caller]
    fn until_running(&self, job: &str) {
        self.placed(job);
        self.delivered();
        self.beat(HELD);
        self.beat(HELD);
        assert_eq!(self.view(job).0, RUNNING);
    }

    /// The signal to an agent started with `--drain-wait 0` that holds one running job: it
    /// stops the job, sends its killed result, and ends. Returns what it printed.
    fn drain_now(self) -> Ran {
        self.agent.signal();
        self.agent.next_wait();
        assert!(self.worker.settle());
        self.agent.elapse();
        self.agent.next_wait();
        assert!(self.worker.settle());
        self.agent.elapse();
        self.agent.finish()
    }
}

/// Asserts a job from the submitter's side to the agent's and back: the job is submitted
/// (201, queued) and, with the host idle, the server's placement poller places it; the start
/// reaches the agent in an answer of the dispatch poll (not in a heartbeat's reply) and the
/// agent creates the container; the heartbeat after lists it, is held (the agent beats
/// again at once) and the job reads running; with the whole budget booked a second job stays
/// queued; when the container exits the next heartbeat carries the result, which the server
/// records (succeeded, reason exit) and acknowledges, and the agent drops it; the second job
/// is then placed on the budget that came back and reads running. It catches a poller that
/// leaves a job on an idle host queued, a start that never reaches the container runtime, a
/// result that never reaches the server or is never acknowledged, a budget that never
/// returns, and a reply with a start that is held, or one with nothing that is not.
#[test]
fn a_job_goes_through_the_agent_and_its_result_comes_back() {
    let rig = rig("jobs_e2e_flow", &["--drain-wait", "0"]);
    let job = rig.queued(4000);
    assert_eq!(rig.server.left_the_queue(&job), "JOB_STATE_PLACED");

    rig.delivered();
    assert_eq!(rig.machine.world.created(), [job.as_str()]);
    rig.beat(HELD);
    assert_eq!(rig.view(&job).0, RUNNING);
    let second = rig.queued(1000);
    rig.beat(HELD);
    assert_eq!(rig.server.job_state(&second), "JOB_STATE_QUEUED");

    rig.machine.world.exit(&job, 0);
    assert!(rig.worker.settle());
    rig.beat(HELD);
    assert_eq!(
        rig.view(&job),
        (
            "JOB_STATE_SUCCEEDED".into(),
            Some("RESULT_REASON_EXIT".into())
        )
    );
    assert!(
        rig.worker.snapshot().results.is_empty(),
        "the ack was taken"
    );
    assert_eq!(rig.machine.world.alive(), 0);

    rig.until_running(&second);
    let ran = rig.drain_now();
    assert_eq!(ran.status(), code(0), "{}", ran.err);
}

/// Asserts a cancel from the submitter: the cancel is taken and reads the job still running
/// with the cancel requested, the dispatch poll hands out the stop, the agent stops the container,
/// and the job ends cancelled. It catches a stop the agent ignores and a killed result the
/// server does not take.
#[test]
fn a_cancel_is_delivered_and_the_job_is_stopped() {
    let rig = rig("jobs_e2e_stop", &[]);
    let job = rig.queued(2000);
    rig.until_running(&job);

    let asked = rig.server.cancel(&job);
    assert_eq!(asked.status, 200, "{}", asked.body);
    assert_eq!(asked.body["job"]["cancelRequested"], true, "{}", asked.body);
    rig.delivered();
    rig.beat(HELD);
    assert_eq!(rig.view(&job).0, "JOB_STATE_CANCELLED");
    let log = rig.machine.world.log();
    let order: Vec<&String> = log.iter().filter(|call| call.ends_with(&job)).collect();
    let expected = ["create", "stop", "remove"].map(|call| format!("{call} {job}"));
    assert_eq!(order, expected.iter().collect::<Vec<_>>());
    rig.agent.stop();
}

/// Asserts the usage through the agent as the program assembles it, against a real server:
/// with `--actions-cgroup` the heartbeats the agent posts carry a load once a job is
/// running (the node's memory and CPU counters), and the server goes on accepting them (the
/// job reads running). It catches a flag that is checked and never reaches the
/// worker and the session, and a load the real server would refuse.
#[test]
fn the_agent_sends_its_load_to_a_real_server() {
    let server = Server::start_pool("jobs_e2e_load", &[], POOL_RUNS_QUICK_TOML);
    let address = Arc::new(Mutex::new(server.url()));
    let posts = Recorder::default();
    let posted = posts.0.clone();
    let slice = "/sys/fs/cgroup/jobs.slice";
    let flags = ["--actions-cgroup", slice, "--drain-wait", "0"];
    let rig = rig_over(server, address, "jobs_e2e_load", &flags, move |_| {
        posts.clone()
    });
    rig.machine.world.set_cgroup_base(slice);
    let run = rig.queued(1000);
    let files = &rig.machine.files;
    files.set(&format!("{slice}/cpu.stat"), "usage_usec 10\n");
    files.set(&format!("{slice}/memory.current"), "4096\n");
    files.set(&format!("{slice}/cpu.max"), "max 100000\n");
    files.set(
        &format!("{slice}/memory.stat"),
        "inactive_file 0\nactive_file 0\n",
    );
    rig.machine.clock.advance(5_000);
    rig.until_running(&run);
    rig.beat(HELD);

    let loads: Vec<Value> = posted
        .lock()
        .unwrap()
        .iter()
        .filter_map(|beat| beat.get("load").cloned())
        .collect();
    assert!(!loads.is_empty(), "no heartbeat carried a load");
    assert_eq!(loads[0]["working_set_bytes"], 4096);
    assert_eq!(loads[0]["cpu_usage_usec"], 10);
    assert_eq!(rig.view(&run).0, RUNNING);
    let ran = rig.drain_now();
    assert_eq!(ran.status(), code(0), "{}", ran.err);
}

/// Asserts that a job outlives a server restart and the agent keeps its session: after the
/// server restarts over its files, the agent's next heartbeat is served from the host's row
/// (the wait stays the interval, not the immediate retry of a refused beat) with no second
/// hello, the host is live, the job (kept in the job store) still reads running and the server
/// sends no stop for it; when the container then exits its result reaches the restarted server.
/// It catches a restart that loses a running job, a restarted server that refuses a beat of a
/// session its rows hold (which costs a hello and a re-adoption of every held job), an agent
/// that heartbeats a dead session for ever, and a server that stops a job it holds.
#[test]
fn a_running_job_survives_a_server_restart_and_its_result_arrives() {
    let rig = rig("jobs_e2e_restart", &[]);
    let job = rig.queued(2000);
    rig.until_running(&job);

    let Rig {
        polls: _,
        server,
        agent,
        worker,
        machine,
        address,
        flags: _,
        files: _files,
    } = rig;
    let server = server.restart();
    *address.lock().unwrap() = server.url();
    // A served beat with nothing to send is held (the wait is `HELD`); a refused one is also
    // an immediate retry, and the hello that follows it waits an interval. Two held waits
    // mean the beat was served and no hello was sent.
    agent.elapse();
    assert_eq!(agent.next_wait(), HELD, "the beat is served");
    assert!(worker.settle());
    assert_eq!(server.host("h1")["state"], "live");
    agent.elapse();
    assert_eq!(agent.next_wait(), HELD, "no hello followed");
    assert!(worker.settle());
    assert_eq!(server.job_state(&job), RUNNING);
    assert!(!machine.world.log().contains(&format!("stop {job}")));

    machine.world.exit(&job, 0);
    assert!(worker.settle());
    agent.elapse();
    agent.next_wait();
    assert_eq!(server.job_state(&job), "JOB_STATE_SUCCEEDED");
    // The server's acknowledgement of the result is in the reply to that heartbeat: the
    // worker takes it before the stop, so the drain finds nothing left to wait for.
    assert!(worker.settle());
    let ran = agent.stop();
    assert!(!ran.err.contains("lost the session"), "{}", ran.err);
}

/// Asserts what a server that forgot a job does to the agent's work: after the server
/// restarts without its job store, the agent says hello again at once with the job in its
/// held list, the server (which has no record of it) answers with a stop, the agent stops the
/// container and sends no result, and the server lists no job. It catches a stop the agent
/// does not obey and a result sent for work the server cancelled.
#[test]
fn a_stop_for_a_job_the_restarted_server_forgot_stops_it_silently() {
    let rig = rig("jobs_e2e_forgot", &[]);
    let job = rig.queued(2000);
    rig.until_running(&job);

    let Rig {
        polls: _,
        server,
        agent,
        worker,
        machine,
        address,
        flags: _,
        files: _files,
    } = rig;
    let server = server.restart_forgetting_jobs();
    *address.lock().unwrap() = server.url();
    agent.elapse();
    assert_eq!(
        agent.next_wait(),
        Duration::ZERO,
        "the heartbeat is refused"
    );
    agent.elapse();
    assert_eq!(agent.next_wait(), INTERVAL, "the hello");
    assert!(worker.settle());

    agent.elapse();
    agent.next_wait();
    assert!(worker.settle());
    assert!(machine.world.log().contains(&format!("stop {job}")));
    assert_eq!(machine.world.alive(), 0);
    assert!(
        worker.snapshot().results.is_empty(),
        "a stopped job the server forgot sends no result"
    );

    agent.elapse();
    agent.next_wait();
    assert_eq!(server.job_get(&format!("/v1/jobs/{job}")).status, 404);
    // The worker has taken the reply to that heartbeat before the stop, so the drain finds
    // nothing left to wait for.
    assert!(worker.settle());
    agent.stop();
}

/// Asserts the taint's path end to end: a container that cannot be removed taints the host;
/// the next heartbeat carries the taint, the server records it (the host lists it) and
/// acknowledges it, the agent stops reporting it, and the next job is accepted and stays
/// queued while the host is tainted, with nothing more created on it. It catches a taint that
/// never reaches the server, one reported for ever, a server that goes on placing work on a
/// host that found residue, and one that refuses the job instead of letting it wait.
#[test]
fn a_taint_is_reported_acknowledged_and_new_jobs_are_not_placed() {
    let rig = rig("jobs_e2e_taint", &[]);
    let run = rig.queued(2000);
    rig.until_running(&run);
    assert!(rig.server.host("h1").get("taint").is_none());

    rig.machine.world.fail("remove", u32::MAX);
    rig.machine.world.exit(&run, 0);
    assert!(rig.worker.settle());
    assert!(rig.worker.snapshot().taint.is_some());
    rig.beat(HELD);

    let host = rig.server.host("h1");
    assert_eq!(host["taint"]["reason"], "cleanup-failed");
    assert_eq!(host["taint"]["run_id"], run.as_str());
    assert!(
        rig.worker.snapshot().taint.is_none(),
        "the agent was told it is known"
    );
    // A tainted host stays capable at admission (its taint ends with a clear), so the job is
    // accepted and waits: placement skips the tainted host, and the next heartbeats start
    // nothing on it.
    let waiting = rig.queued(1000);
    for _ in 0..3 {
        rig.agent.elapse();
        rig.agent.next_wait();
        assert!(rig.worker.settle());
    }
    assert_eq!(rig.server.job_state(&waiting), "JOB_STATE_QUEUED");
    assert_eq!(rig.machine.world.created(), [run.as_str()]);
    // The job that left the container behind stays held for as long as the agent lives, so
    // the agent is not drained here: dropping the rig ends it.
}

/// Asserts that the heartbeats go on while the container runtime does not answer: with the
/// runtime held inside the start of a job, twenty more heartbeats are posted and answered,
/// counted at the transport and not timed, and the start (sent again each time, since the
/// agent does not list the job) is dropped with a note once the worker's queue is full;
/// from then on each heartbeat waits a backoff longer than the last instead of the interval;
/// when the runtime answers the job is created once and listed. It catches a session that
/// calls the runtime itself, so that a slow engine would stop the heartbeats and the server
/// would give the host up, and one that asks a full worker again at once.
#[test]
fn the_heartbeats_go_on_while_the_runtime_is_slow() {
    let server = Server::start_pool("jobs_e2e_slow", &[], POOL_RUNS_QUICK_TOML);
    let address = Arc::new(Mutex::new(server.url()));
    let beats = Counting::default();
    let counted = beats.0.clone();
    let flags = ["--drain-wait", "0"];
    let rig = rig_over(server, address, "jobs_e2e_slow", &flags, move |_| {
        beats.clone()
    });
    // Held before the job exists: the dispatch poller can hand the start to the worker as soon
    // as the job is queued, and a create that began before the hold was armed would not wait.
    rig.machine.world.hold_creates();
    let run = rig.queued(2000);

    for _ in 0..2 {
        rig.agent.elapse();
        rig.agent.next_wait();
    }
    rig.machine.world.wait_entered(1);
    let before = counted.load(Ordering::SeqCst);
    let waits: Vec<Duration> = (0..20)
        .map(|_| {
            rig.agent.elapse();
            rig.agent.next_wait()
        })
        .collect();
    assert_eq!(counted.load(Ordering::SeqCst) - before, 20);
    let full = waits.iter().position(|wait| *wait != INTERVAL);
    let full = full.expect("the worker's queue filled");
    assert!(
        waits[..full].iter().all(|wait| *wait == INTERVAL),
        "{waits:?}"
    );
    assert!(
        waits[full..].windows(2).all(|pair| pair[0] < pair[1]),
        "{waits:?}"
    );
    assert!(waits[full] > Duration::ZERO, "{waits:?}");
    assert!(
        rig.machine.world.created().is_empty(),
        "the runtime has not answered"
    );

    rig.machine.world.release();
    assert!(rig.worker.settle());
    assert_eq!(rig.machine.world.created(), [run.as_str()]);
    rig.beat(HELD);
    assert_eq!(rig.view(&run).0, RUNNING);
    let ran = rig.drain_now();
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    assert!(ran.err.contains("the job worker is busy"), "{}", ran.err);
}

/// Asserts the shutdown of an agent with a job: at the signal the agent announces `draining`
/// (the host takes no new job) and keeps heartbeating; the job ends by itself, its result
/// reaches the server, which records it, and the agent ends with status 0 once a heartbeat
/// has listed nothing, without having stopped the job. It catches an agent that leaves at the
/// signal with a job running and its result unsent.
#[test]
fn sigterm_lets_the_job_finish_and_reports_it_before_the_agent_leaves() {
    let rig = rig("jobs_e2e_term", &["--drain-wait", "3600"]);
    let run = rig.queued(2000);
    rig.until_running(&run);

    rig.agent.signal();
    assert_eq!(
        rig.agent.next_wait(),
        INTERVAL,
        "the first draining heartbeat"
    );
    let host = rig.server.host("h1");
    assert_eq!(host["phase"], "draining");
    assert_eq!(rig.view(&run).0, RUNNING);
    let waiting = rig.queued(1000);
    assert_eq!(
        rig.server.job_state(&waiting),
        "JOB_STATE_QUEUED",
        "a draining host takes no new job"
    );

    rig.machine.world.exit(&run, 0);
    assert!(rig.worker.settle());
    rig.agent.elapse();
    assert_eq!(rig.agent.next_wait(), INTERVAL, "the result goes up");
    assert_eq!(
        rig.view(&run),
        (
            "JOB_STATE_SUCCEEDED".into(),
            Some("RESULT_REASON_EXIT".into())
        )
    );
    assert!(rig.worker.settle());
    rig.agent.elapse();

    let Rig {
        agent,
        machine,
        server,
        ..
    } = rig;
    let ran = agent.finish();
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    assert!(!machine.world.log().contains(&format!("stop {run}")));
    assert_eq!(server.host("h1")["phase"], "draining");
    assert_eq!(server.job_state(&waiting), "JOB_STATE_QUEUED");
}

/// Asserts that a job still running when the drain's wait is over is stopped and its killed
/// result reaches the server before the agent ends (`--drain-wait 0`: no wait). It catches a
/// drain that leaves a job running when the agent exits, and one that exits before the
/// result of the job it stopped is acknowledged.
#[test]
fn sigterm_with_no_wait_stops_the_job_and_reports_it_killed() {
    let rig = rig("jobs_e2e_term_now", &["--drain-wait", "0"]);
    let run = rig.queued(2000);
    rig.until_running(&run);

    rig.agent.signal();
    assert_eq!(rig.agent.next_wait(), INTERVAL);
    assert!(rig.worker.settle());
    rig.agent.elapse();
    assert_eq!(rig.agent.next_wait(), INTERVAL, "the killed result goes up");
    assert_eq!(rig.view(&run).0, "JOB_STATE_FAILED");
    assert!(rig.worker.settle());
    rig.agent.elapse();

    let Rig { agent, machine, .. } = rig;
    let ran = agent.finish();
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    assert!(machine.world.log().contains(&format!("stop {run}")));
    assert_eq!(machine.world.alive(), 0);
}

/// Asserts that a cordoned host is placed nothing until it is uncordoned: with the host of a
/// container agent cordoned through the operator route, a submitted job stays queued across
/// heartbeats and the runtime creates nothing; once the host is uncordoned the job is
/// started and reads running. It catches a placement that ignores the cordon and a cordon that
/// is never lifted for placement.
#[test]
fn a_cordoned_host_is_placed_no_job_until_it_is_uncordoned() {
    let rig = rig("jobs_e2e_cordon", &["--drain-wait", "0"]);
    let cordoned = rig.server.job_post("/v1/hosts/h1:cordon", &json!({}));
    assert_eq!(cordoned.status, 200, "{}", cordoned.body);
    let job = rig.queued(1000);
    rig.beat(HELD);
    rig.beat(HELD);
    assert_eq!(rig.server.job_state(&job), "JOB_STATE_QUEUED");
    assert!(rig.machine.world.created().is_empty());

    let lifted = rig.server.job_post("/v1/hosts/h1:uncordon", &json!({}));
    assert_eq!(lifted.status, 200, "{}", lifted.body);
    rig.until_running(&job);
    assert_eq!(rig.machine.world.created(), [job.as_str()]);
    let ran = rig.drain_now();
    assert_eq!(ran.status(), code(0), "{}", ran.err);
}

/// Asserts an agent update over a busy host, end to end against the real server: two jobs
/// run; the update's request (the handover file in the state root) and the stop end the agent
/// with status 0 and stop no job; one job ends while no agent runs; the next process of the
/// agent, started once the server has stopped counting the old session as live, says hello,
/// takes both jobs over and lists them, and the server records the job that ended in the gap
/// as failed with its own exit and the other, when it ends, as succeeded; each result is
/// acknowledged once and nothing is started twice. It catches an update that stops the jobs or
/// drains, a next agent that loses a job or its result, and a result sent under another job.
#[test]
fn an_update_hands_the_running_jobs_to_the_next_agent_and_each_result_reaches_the_server() {
    let rig = rig("jobs_e2e_handover", &[]);
    let first = rig.queued(1000);
    rig.until_running(&first);
    let second = rig.queued(1000);
    rig.until_running(&second);

    js_agent::lifecycle::handover::write(&rig.files.path("state")).unwrap();
    rig.agent.signal();
    let Rig {
        server,
        agent,
        machine,
        address,
        flags,
        files,
        ..
    } = rig;
    let ran = agent.finish();
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    assert!(
        ran.err
            .contains("js-agent: handed over: every run is left as it is to the next process"),
        "{}",
        ran.err
    );
    assert_eq!(machine.world.alive(), 2);
    assert!(
        !machine
            .world
            .log()
            .iter()
            .any(|call| call.starts_with("stop ")),
        "{:?}",
        machine.world.log()
    );

    // The second job ends while no agent runs; the server's clock passes its grace.
    machine.world.exit(&second, 3);
    server.clock.advance(Duration::from_secs(61));
    let (agent, worker, polls) = launch(&server, &address, &flags, &machine, Redirect);
    let rig = Rig {
        polls,
        server,
        agent,
        worker,
        machine,
        address,
        flags,
        files,
    };
    assert!(
        !rig.files.path("state").join("handover").exists(),
        "the next agent removed nothing it was asked: there was nothing left"
    );
    rig.tick();
    rig.tick();
    assert_eq!(rig.view(&first).0, RUNNING);
    assert_eq!(
        rig.view(&second),
        ("JOB_STATE_FAILED".into(), Some("RESULT_REASON_EXIT".into()))
    );
    rig.machine.world.exit(&first, 0);
    assert!(rig.worker.settle());
    rig.tick();
    assert_eq!(
        rig.view(&first),
        (
            "JOB_STATE_SUCCEEDED".into(),
            Some("RESULT_REASON_EXIT".into())
        )
    );
    assert!(
        rig.worker.snapshot().results.is_empty(),
        "both acknowledged"
    );
    assert_eq!(
        rig.machine.world.created(),
        [first.as_str(), second.as_str()]
    );
    let ran = rig.agent.stop();
    assert_eq!(ran.status(), code(0), "{}", ran.err);
}

/// Asserts what the next process does with a handover file the last stop left (an update step
/// killed after it wrote the file): it removes it at its start and says so, so its own stop
/// drains; and a file it cannot remove is named in a warning and the agent still starts.
/// Catches a request made of one process that a later one acts on unasked.
#[test]
fn an_agent_removes_a_handover_file_left_by_an_earlier_stop() {
    for (name, make, said) in [
        (
            "jobs_e2e_handover_left",
            (|path: &Path| js_agent::lifecycle::handover::write(path).unwrap()) as fn(&Path),
            "js-agent: removed a handover file left by an earlier stop",
        ),
        (
            "jobs_e2e_handover_stuck",
            |path: &Path| std::fs::create_dir(path.join("handover")).unwrap(),
            "js-agent: warning: a handover file left by an earlier stop cannot be removed (",
        ),
    ] {
        let server = Server::start_pool(name, &[], POOL_RUNS_QUICK_TOML);
        let address = Arc::new(Mutex::new(server.url()));
        let files = Scratch::new(&format!("{name}-files"));
        make(&private(&files, "state"));
        let rig = rig_in(server, address, files, &["--drain-wait", "0"], Redirect);
        let ran = rig.agent.stop();
        assert!(ran.err.contains(said), "{name}: {}", ran.err);
    }
}
