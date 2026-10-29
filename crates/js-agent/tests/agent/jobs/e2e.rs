//! A real lease server in the test process and a real agent session and worker on a thread
//! of their own, over a container runtime in memory and a clock the test moves. The agent's
//! waits are opened one at a time by the test, so each test reads as the order of events
//! it asserts: nothing here waits for a time to pass.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use js_agent::Handle;
use js_client::Reply;
use serde_json::{Value, json};

use super::fake::FakeMachine;
use super::{private, user};
use crate::server::{Agent, Counting, POOL_RUNS_TOML, Redirect, Server};
use crate::support::{Ran, Scratch, code, linux_host};

/// A server with one agent that runs containers, and a lease on its host.
struct Rig {
    server: Server,
    agent: Agent,
    worker: Handle,
    machine: Arc<FakeMachine>,
    lease: String,
    address: Arc<Mutex<String>>,
    _files: Scratch,
}

/// Starts the server and the agent (`extra` flags beside the container ones), waits until
/// the agent has said hello, and takes a lease on its host.
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
        "--container-bin",
        &exec,
        "--container-user",
        &user,
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
    let body = json!({
        "class": "mac",
        "resources": { "cpu_millis": 1000, "memory_bytes": 1024 },
        "capabilities": { "os": "linux" },
        "max_wall_seconds": 3600,
    });
    let granted = server.post_body("/v1/leases", &body);
    assert_eq!(granted.status, 201, "{}", granted.body);
    let lease = granted.body["id"].as_str().unwrap().to_owned();
    Rig {
        server,
        agent,
        worker,
        machine,
        lease,
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

    /// Queues a run of `cpu` thousandths of a core.
    fn queue(&self, cpu: u64) -> Reply {
        let body = json!({
            "image": "busybox:1@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "args": ["true"],
            "limits": { "cpu_millis": cpu, "memory_bytes": 1048576 },
        });
        let path = format!("/v1/leases/{}/runs", self.lease);
        self.server.post_body(&path, &body)
    }

    /// Queues a run that is expected to be taken; returns its id.
    fn queued(&self, cpu: u64) -> String {
        let reply = self.queue(cpu);
        assert_eq!(reply.status, 202, "{}", reply.body);
        reply.body["run_id"].as_str().unwrap().to_owned()
    }

    /// The listing of the lease's runs.
    fn runs(&self) -> Vec<Value> {
        let listing = self.server.get(&format!("/v1/leases/{}/runs", self.lease));
        listing["runs"].as_array().unwrap().clone()
    }

    /// What the listing says of `run`: its state, and its outcome when it has one.
    fn view(&self, run: &str) -> (String, Option<String>) {
        let runs = self.runs();
        let entry = runs.iter().find(|entry| entry["run_id"] == run);
        let entry = entry.unwrap_or_else(|| panic!("{run} is not listed: {runs:?}"));
        let outcome = entry["outcome"].as_str().map(str::to_owned);
        (entry["state"].as_str().unwrap().to_owned(), outcome)
    }

    /// Beats until the server lists `run` as running.
    fn until_running(&self, run: &str) {
        self.beat();
        self.beat();
        assert_eq!(self.view(run).0, "running");
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

/// Asserts a run from the lease holder's side to the agent's and back: the run is queued
/// (202); the next heartbeat's reply carries its start and the agent creates the container;
/// the heartbeat after lists it and the run reads running; with the whole budget booked a
/// second run is refused (409 `no-capacity`); when the container exits the next heartbeat
/// carries the result, which the server records (exited, succeeded) and acknowledges, and
/// the agent drops it; the heartbeat after lists nothing, the run is ended, and the budget
/// is back. It catches a start that never reaches the container runtime, a result that
/// never reaches the server or is never acknowledged, and a budget that never returns.
#[test]
fn a_run_goes_through_the_agent_and_its_result_comes_back() {
    let rig = rig("jobs_e2e_flow", &[]);
    let run = rig.queued(4000);
    assert_eq!(rig.view(&run), ("queued".into(), None));

    rig.beat();
    assert_eq!(rig.machine.world.created(), [run.as_str()]);
    assert_eq!(rig.view(&run).0, "queued");
    rig.beat();
    assert_eq!(rig.view(&run).0, "running");
    let full = rig.queue(1000);
    assert_eq!(
        (full.status, full.body),
        (409, json!({ "error": "no-capacity" }))
    );

    rig.machine.world.exit(&run, 0);
    assert!(rig.worker.settle());
    rig.beat();
    assert_eq!(rig.view(&run), ("exited".into(), Some("succeeded".into())));
    assert!(
        rig.worker.snapshot().results.is_empty(),
        "the ack was taken"
    );
    assert_eq!(rig.machine.world.alive(), 0);

    rig.beat();
    assert_eq!(rig.view(&run).0, "ended");
    assert_eq!(rig.queue(4000).status, 202, "the budget is back");
    let ran = rig.agent.stop();
    assert_eq!(ran.status(), code(0), "{}", ran.err);
}

/// Asserts a stop from the lease holder: the delete is taken (202), the next reply carries
/// the stop, the agent stops the container, and the result that follows says `killed`. It
/// catches a stop the agent ignores and a killed result the server does not take.
#[test]
fn a_stop_is_delivered_and_the_run_is_killed() {
    let rig = rig("jobs_e2e_stop", &[]);
    let run = rig.queued(2000);
    rig.until_running(&run);

    let path = format!("/v1/leases/{}/runs/{run}", rig.lease);
    assert_eq!(rig.server.delete(&path).status, 202);
    rig.beat();
    rig.beat();
    assert_eq!(rig.view(&run), ("exited".into(), Some("killed".into())));
    let log = rig.machine.world.log();
    let order: Vec<&String> = log.iter().filter(|call| call.ends_with(&run)).collect();
    let expected = ["create", "stop", "remove"].map(|call| format!("{call} {run}"));
    assert_eq!(order, expected.iter().collect::<Vec<_>>());
    rig.agent.stop();
}

/// Asserts what a server that forgot a run does to the agent's work: after the server
/// restarts, the agent says hello again at once with the run in its held list, the server
/// (which has no record of it, though the lease survived) answers with a cancel, the agent
/// stops the container and sends no result, and the listing of the lease's runs is empty.
/// It catches a cancel the agent does not obey and a result sent for work the server
/// cancelled.
#[test]
fn a_cancel_for_a_run_the_restarted_server_forgot_stops_it_silently() {
    let rig = rig("jobs_e2e_cancel", &[]);
    let run = rig.queued(2000);
    rig.until_running(&run);

    let Rig {
        server,
        agent,
        worker,
        machine,
        lease,
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

    agent.elapse();
    agent.next_wait();
    assert!(worker.settle());
    assert!(machine.world.log().contains(&format!("stop {run}")));
    assert_eq!(machine.world.alive(), 0);
    assert!(
        worker.snapshot().results.is_empty(),
        "a cancelled run sends no result"
    );

    agent.elapse();
    agent.next_wait();
    let listing = server.get(&format!("/v1/leases/{lease}/runs"));
    assert_eq!(listing["runs"], json!([]));
    agent.stop();
}

/// Asserts the taint's path end to end: a container that cannot be removed taints the host;
/// the next heartbeat carries the taint, the server records it (the host lists it) and
/// acknowledges it, the agent stops reporting it, and the lease holder's next run is refused
/// (409 `host-tainted`). It catches a taint that never reaches the server, one reported for
/// ever, and a server that goes on placing work on a host that found residue.
#[test]
fn a_taint_is_reported_acknowledged_and_new_runs_are_refused() {
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
        (409, json!({ "error": "host-tainted" }))
    );
    // The job that left the container behind stays held for as long as the agent lives, so
    // the agent is not drained here: dropping the rig ends it.
}

/// Asserts that the heartbeats go on while the container runtime does not answer: with the
/// runtime held inside the start of a run, twenty more heartbeats are posted and answered,
/// counted at the transport and not timed, and the start (sent again each time, since the
/// agent does not list the run) is dropped with a note once the worker's queue is full;
/// when the runtime answers the run is created once and listed. It catches a session that
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

    rig.agent.elapse();
    rig.agent.next_wait();
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
    assert_eq!(rig.view(&run).0, "running");
    let ran = rig.drain_now();
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    assert!(ran.err.contains("the job worker is busy"), "{}", ran.err);
}

/// Asserts the shutdown of an agent with a job: at the signal the agent announces `draining`
/// (the host takes no new lease) and keeps heartbeating; the job ends by itself, its result
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
    assert_eq!(rig.view(&run).0, "running");

    rig.machine.world.exit(&run, 0);
    assert!(rig.worker.settle());
    rig.agent.elapse();
    assert_eq!(
        rig.agent.next_wait(),
        Duration::from_secs(5),
        "the result goes up"
    );
    assert_eq!(rig.view(&run), ("exited".into(), Some("succeeded".into())));
    assert!(rig.worker.settle());
    rig.agent.elapse();

    let Rig {
        agent,
        machine,
        server,
        lease,
        ..
    } = rig;
    let ran = agent.finish();
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    assert!(!machine.world.log().contains(&format!("stop {run}")));
    assert_eq!(server.host("h1")["phase"], "draining");
    assert_eq!(
        server.get(&format!("/v1/leases/{lease}/runs"))["runs"][0]["state"],
        "ended"
    );
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
    assert_eq!(rig.view(&run), ("exited".into(), Some("killed".into())));
    assert!(rig.worker.settle());
    rig.agent.elapse();

    let Rig { agent, machine, .. } = rig;
    let ran = agent.finish();
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    assert!(machine.world.log().contains(&format!("stop {run}")));
    assert_eq!(machine.world.alive(), 0);
}
