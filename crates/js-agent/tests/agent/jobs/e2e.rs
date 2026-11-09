//! A real server in the test process and a real agent session and worker on a thread
//! of their own, over a container runtime in memory and a clock the test moves. The agent's
//! waits are opened one at a time by the test, so each test reads as the order of events
//! it asserts: nothing here waits for a time to pass.
//!
//! The jobs here are submitted through the v1 job API (`POST /v1/jobs` on `--job-listen`) and
//! placed by `js-sched` on the agent's host. The drain that lets a running job finish is also
//! asserted with the server's own binary and the crun runtime, in js-lease-server's
//! `v1_acceptance` target.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use js_agent::Handle;
use js_client::Reply;
use serde_json::{Value, json};

use super::fake::FakeMachine;
use super::{private, user};
use crate::server::{Agent, Counting, POOL_RUNS_TOML, Recorder, Redirect, Server};
use crate::support::{Ran, Scratch, code, linux_host};

/// The state of a job that is running, as the job service names it.
const RUNNING: &str = "JOB_STATE_RUNNING";

/// A server with one agent that runs containers.
struct Rig {
    server: Server,
    agent: Agent,
    worker: Handle,
    machine: Arc<FakeMachine>,
    address: Arc<Mutex<String>>,
    _files: Scratch,
}

/// Starts the server and the agent (`extra` flags beside the container ones), waits until
/// the agent has said hello.
fn rig(name: &str, extra: &[&str]) -> Rig {
    let server = Server::start_pool(name, &[], POOL_RUNS_TOML);
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
    let exec = files.write("docker", "#!/bin/sh\n", 0o755);
    let (user, _, _) = user();
    let state = private(&files, "state").display().to_string();
    let scratch = private(&files, "run-scratch").display().to_string();
    let exec = exec.display().to_string();
    let mut flags = vec![
        "--host-id",
        "h1",
        "--runtime",
        "docker",
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
    ];
    flags.extend_from_slice(extra);
    let machine = Arc::new(FakeMachine::new());
    let agent = Agent::spawn_on(
        server.url(),
        server.join_file(),
        &flags,
        linux_host,
        connector(address.clone()),
        machine.clone(),
    );
    let worker = agent.worker();
    assert_eq!(agent.next_wait(), Duration::from_secs(5), "the hello");
    Rig {
        server,
        agent,
        worker,
        machine,
        address,
        _files: files,
    }
}

impl Rig {
    /// One heartbeat of the agent, and the worker done with the reply to it.
    fn beat(&self) {
        self.agent.elapse();
        assert_eq!(self.agent.next_wait(), Duration::from_secs(5));
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

    /// Beats until the server lists `job` as running.
    fn until_running(&self, job: &str) {
        self.beat();
        self.beat();
        self.beat();
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
/// (201) and reads queued; the next heartbeat's reply carries its start and the agent creates
/// the container; the heartbeat after lists it and the job reads running; with the whole
/// budget booked a second job stays queued; when the container exits the next heartbeat
/// carries the result, which the server records (succeeded, reason exit) and acknowledges,
/// and the agent drops it; the heartbeat after lists nothing, and the queued job is placed on
/// the budget that came back. It catches a start that never reaches the container runtime, a
/// result that never reaches the server or is never acknowledged, and a budget that never
/// returns.
#[test]
fn a_job_goes_through_the_agent_and_its_result_comes_back() {
    let rig = rig("jobs_e2e_flow", &["--drain-wait", "0"]);
    let job = rig.queued(4000);
    assert_eq!(rig.view(&job).0, "JOB_STATE_QUEUED");

    rig.beat();
    assert_eq!(rig.view(&job).0, "JOB_STATE_PLACED");
    rig.beat();
    assert_eq!(rig.machine.world.created(), [job.as_str()]);
    rig.beat();
    assert_eq!(rig.view(&job).0, RUNNING);
    let second = rig.queued(1000);
    rig.beat();
    assert_eq!(rig.server.job_state(&second), "JOB_STATE_QUEUED");

    rig.machine.world.exit(&job, 0);
    assert!(rig.worker.settle());
    rig.beat();
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
/// with the cancel requested, the next reply carries the stop, the agent stops the container,
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
    rig.beat();
    rig.beat();
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
    let server = Server::start_pool("jobs_e2e_load", &[], POOL_RUNS_TOML);
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
    rig.beat();

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

/// Asserts that a job outlives a server restart: after the server restarts over its files,
/// the agent's next heartbeat is refused as an unknown host, it says hello again at once with
/// the job in its held list, the server (which kept the job in its store) adopts it and the
/// job still reads running; when the container then exits its result reaches the restarted
/// server. It catches a restart that loses a running job, an agent that heartbeats a dead
/// session for ever, and a hello that makes the server stop a job it holds.
#[test]
fn a_running_job_survives_a_server_restart_and_its_result_arrives() {
    let rig = rig("jobs_e2e_restart", &[]);
    let job = rig.queued(2000);
    rig.until_running(&job);

    let Rig {
        server,
        agent,
        worker,
        machine,
        address,
        _files,
    } = rig;
    let server = server.restart();
    *address.lock().unwrap() = server.url();
    agent.elapse();
    assert_eq!(
        agent.next_wait(),
        Duration::ZERO,
        "the heartbeat is refused"
    );
    agent.elapse();
    assert_eq!(agent.next_wait(), Duration::from_secs(5), "the hello");
    assert!(worker.settle());
    assert_eq!(server.host("h1")["state"], "live");

    agent.elapse();
    agent.next_wait();
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
    agent.stop();
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
        server,
        agent,
        worker,
        machine,
        address,
        _files,
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
    assert_eq!(agent.next_wait(), Duration::from_secs(5), "the hello");
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
/// acknowledges it, the agent stops reporting it, and the next job stays queued while the
/// host is tainted. It catches a taint that never reaches the server, one reported for
/// ever, and a server that goes on placing work on a host that found residue.
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
    rig.beat();

    let host = rig.server.host("h1");
    assert_eq!(host["taint"]["reason"], "cleanup-failed");
    assert_eq!(host["taint"]["run_id"], run.as_str());
    assert!(
        rig.worker.snapshot().taint.is_none(),
        "the agent was told it is known"
    );
    let refused = rig.queue(1000);
    assert_eq!(
        (refused.status, refused.body),
        (422, json!({ "error": "no-capable-host" }))
    );
    // The job that left the container behind stays held for as long as the agent lives, so
    // the agent is not drained here: dropping the rig ends it.
}

/// Asserts that the heartbeats go on while the container runtime does not answer: with the
/// runtime held inside the start of a job, twenty more heartbeats are posted and answered,
/// counted at the transport and not timed, and the start (sent again each time, since the
/// agent does not list the job) is dropped with a note once the worker's queue is full;
/// when the runtime answers the job is created once and listed. It catches a session that
/// calls the runtime itself, so that a slow engine would stop the heartbeats and the server
/// would give the host up.
#[test]
fn the_heartbeats_go_on_while_the_runtime_is_slow() {
    let server = Server::start_pool("jobs_e2e_slow", &[], POOL_RUNS_TOML);
    let address = Arc::new(Mutex::new(server.url()));
    let beats = Counting::default();
    let counted = beats.0.clone();
    let flags = ["--drain-wait", "0"];
    let rig = rig_over(server, address, "jobs_e2e_slow", &flags, move |_| {
        beats.clone()
    });
    let run = rig.queued(2000);
    rig.machine.world.hold_creates();

    for _ in 0..2 {
        rig.agent.elapse();
        rig.agent.next_wait();
    }
    rig.machine.world.wait_entered(1);
    let before = counted.load(Ordering::SeqCst);
    for _ in 0..20 {
        rig.agent.elapse();
        assert_eq!(rig.agent.next_wait(), Duration::from_secs(5));
    }
    assert_eq!(counted.load(Ordering::SeqCst) - before, 20);
    assert!(
        rig.machine.world.created().is_empty(),
        "the runtime has not answered"
    );

    rig.machine.world.release();
    assert!(rig.worker.settle());
    assert_eq!(rig.machine.world.created(), [run.as_str()]);
    rig.beat();
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
        Duration::from_secs(5),
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
    assert_eq!(
        rig.agent.next_wait(),
        Duration::from_secs(5),
        "the result goes up"
    );
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
    assert_eq!(rig.agent.next_wait(), Duration::from_secs(5));
    assert!(rig.worker.settle());
    rig.agent.elapse();
    assert_eq!(
        rig.agent.next_wait(),
        Duration::from_secs(5),
        "the killed result goes up"
    );
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
    rig.beat();
    rig.beat();
    assert_eq!(rig.server.job_state(&job), "JOB_STATE_QUEUED");
    assert!(rig.machine.world.created().is_empty());

    let lifted = rig.server.job_post("/v1/hosts/h1:uncordon", &json!({}));
    assert_eq!(lifted.status, 200, "{}", lifted.body);
    rig.until_running(&job);
    assert_eq!(rig.machine.world.created(), [job.as_str()]);
    let ran = rig.drain_now();
    assert_eq!(ran.status(), code(0), "{}", ran.err);
}
