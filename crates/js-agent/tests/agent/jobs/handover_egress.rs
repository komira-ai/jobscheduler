//! An agent update under an egress ceiling, end to end: a crun agent run with
//! `--egress allow-list` over a real lease server leaves its runs, and their network wiring,
//! to the next agent of the host, which takes the wiring over through the egress port. The
//! host's firewall is a port that records its calls (a test that is not root cannot set one
//! up: the machine supplies it in place of the one the agent would start), the containers are
//! the stand-in `crun` of `crun_e2e`, and each result is read from the server. Nothing waits
//! for a time: the heartbeats are the progress events.

use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use js_agent::crun::{EgressPort, Seams, Wired};
use js_agent::egress::EgressError;
use js_agent::{Snapshot, System};
use js_agent_wire::NetworkCeiling;
use js_contain::oci::recorded_supervisor;
use js_types::{JobNetwork, PortMap};
use serde_json::json;

use super::crun_e2e::{NODE_IN_USE, Rig, crun_flags, node, seams};
use super::crun_machine::CrunMachine;
use super::dispatch::GUARD;
use crate::server::QUICK_INTERVAL;
use crate::support::code;

/// The host's egress as the runtime sees it, recording every call that changes a run's
/// wiring. Both agents of a test share it, as both would share the host.
#[derive(Debug, Default)]
struct RecordingPort {
    calls: Mutex<Vec<String>>,
    noted: Condvar,
    refuse_adopt: AtomicBool,
}

impl RecordingPort {
    fn note(&self, call: String) {
        self.calls.lock().expect("calls").push(call);
        self.noted.notify_all();
    }

    /// Waits until `count` calls are noted: the worker's thread makes some after the agent
    /// has ended, which is the event the test waits for (the guard only ends a hang).
    #[track_caller]
    fn wait_for(&self, count: usize) {
        let calls = self.calls.lock().expect("calls");
        let waited = self
            .noted
            .wait_timeout_while(calls, GUARD, |calls| calls.len() < count)
            .expect("calls");
        assert!(!waited.1.timed_out(), "only {:?} was noted", waited.0);
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("calls").clone()
    }
}

/// One run's wiring: what is done to it is noted under the run's container name.
struct Wiring {
    port: Arc<RecordingPort>,
    run: String,
}

impl Wired for Wiring {
    fn release(self: Box<Self>) -> Result<(), EgressError> {
        self.port.note(format!("release {}", self.run));
        Ok(())
    }

    fn leave(self: Box<Self>) {
        self.port.note(format!("leave {}", self.run));
    }
}

/// The port the runtime holds: the shared recorder, which the wirings it makes point back to.
#[derive(Debug)]
struct Port(Arc<RecordingPort>);

impl EgressPort for Port {
    fn check(&self) -> Result<(), EgressError> {
        Ok(())
    }

    fn ceiling(&self) -> NetworkCeiling {
        NetworkCeiling {
            level: JobNetwork::AllowList,
            ports: Vec::new(),
        }
    }

    fn attach(
        &self,
        run: &str,
        _pid: u32,
        level: JobNetwork,
        ports: &[PortMap],
    ) -> Result<Box<dyn Wired>, EgressError> {
        self.0.note(format!("attach {run} {level} {}", ports.len()));
        Ok(Box::new(Wiring {
            port: self.0.clone(),
            run: run.to_owned(),
        }))
    }

    fn adopt(
        &self,
        run: &str,
        level: JobNetwork,
        ports: &[PortMap],
    ) -> Result<Box<dyn Wired>, EgressError> {
        self.0.note(format!("adopt {run} {level} {}", ports.len()));
        if self.0.refuse_adopt.load(Ordering::SeqCst) {
            return Err(EgressError::Unadoptable {
                run: run.to_owned(),
                why: "the record is not the one kept".to_owned(),
            });
        }
        Ok(Box::new(Wiring {
            port: self.0.clone(),
            run: run.to_owned(),
        }))
    }

    fn settle(&self) -> Result<(), EgressError> {
        self.0.note("settle".to_owned());
        Ok(())
    }
}

/// The pool of the agents here: a host joins it by reporting the allow-list ceiling.
const POOL_ALLOW_LIST_TOML: &str = r#"
heartbeat_interval = "250ms"
grace = "1m"
name = "macs"
class = "mac"
max_machines = 4

[shape]
cpu_millis = 4000
memory_bytes = 4294967296

[capabilities]
os = "linux"
arch = "x86_64"
egress = "allow-list"
"#;

/// The flags of an agent of the node that runs with an allow-list ceiling.
fn flags() -> Vec<String> {
    crun_flags(
        "h1",
        &[
            "--egress",
            "allow-list",
            "--egress-allow",
            "192.0.2.10:443",
            "--egress-ip",
            "/bin/true",
            "--egress-nft",
            "/bin/true",
            "--egress-nsenter",
            "/bin/true",
        ],
    )
}

/// The machine of the node whose egress is `port`.
fn machine(port: &Arc<RecordingPort>) -> Arc<CrunMachine> {
    let egress: Arc<dyn EgressPort> = Arc::new(Port(port.clone()));
    let crun = Seams {
        egress: Some(egress),
        ..seams()
    };
    CrunMachine::with_system(System::new().with_crun(crun))
}

/// Submits a job that asks for `allow-list`; returns its id.
fn queued(rig: &Rig) -> String {
    let body = json!({
        "spec": {
            "image": format!("registry.example/job:1@sha256:{}", "ab".repeat(32)),
            "args": ["true"],
            "needs": { "cpuMillis": "1000", "memoryBytes": "1048576" },
            "wallLimitSeconds": "3600",
            "network": "NETWORK_LEVEL_ALLOW_LIST",
        }
    });
    let made = rig.server.job_post("/v1/jobs", &body);
    assert_eq!(made.status, 201, "{}", made.body);
    made.body["job"]["id"].as_str().unwrap().to_owned()
}

/// The container name of the job's run, as the stand-in's calls show it.
fn container_of(job: &str) -> String {
    let calls = fs::read_to_string(node().dir.path().join("calls")).unwrap_or_default();
    let line = calls
        .lines()
        .find(|line| line.contains(" run ") && line.contains(job))
        .unwrap_or_else(|| panic!("no `crun run` for {job} in\n{calls}"));
    let name = line.split_whitespace().last().expect("a name");
    name.to_owned()
}

/// Puts the init the stand-in's `crun state` names into the run's cgroup, as `crun` does.
fn place(name: &str) {
    let ctr = node().cgroups.join("actions").join(name).join("ctr");
    fs::create_dir_all(&ctr).expect("the container's cgroup");
    fs::write(ctr.join("cgroup.procs"), "1\n").expect("its processes");
}

/// Waits until the supervisor of the container `name` has ended, which it does after it has
/// written the record of the container's end.
fn supervisor_ended(name: &str) {
    for entry in fs::read_dir(&node().scratch)
        .into_iter()
        .flatten()
        .flatten()
    {
        if !entry.file_name().to_string_lossy().contains(name) {
            continue;
        }
        if let Some(id) = recorded_supervisor(&entry.path()) {
            // A hang guard: the end of the supervisor is what the loop waits for, and it
            // is the only way out but a panic after a very large number of turns.
            let mut turns = 0u64;
            while id.alive() {
                turns += 1;
                assert!(
                    turns < HANG_GUARD_TURNS,
                    "the supervisor of {name} did not end"
                );
                std::thread::yield_now();
            }
        }
    }
}

/// The turns `supervisor_ended` waits at most, far beyond what the end of a supervisor takes.
const HANG_GUARD_TURNS: u64 = 200_000_000;

const RUNNING: &str = "JOB_STATE_RUNNING";

/// Two wired jobs running under an allow-list ceiling; returns the rig and the jobs with
/// their container names, the processes placed.
fn running_pair(name: &str, port: &Arc<RecordingPort>) -> (Rig, [(String, String); 2]) {
    let machine = machine(port);
    let rig = Rig::on_flags(name, flags(), (POOL_ALLOW_LIST_TOML, machine));
    let first = queued(&rig);
    rig.until(&first, |seen| seen.0 == RUNNING);
    let second = queued(&rig);
    rig.until(&second, |seen| seen.0 == RUNNING);
    let jobs = [first, second].map(|job| {
        let container = container_of(&job);
        place(&container);
        (job, container)
    });
    (rig, jobs)
}

/// What is left when an agent has handed over: the server and the machine the next agent
/// runs against.
struct Gap {
    server: crate::server::Server,
    machine: Arc<CrunMachine>,
    flags: Vec<String>,
    address: Arc<Mutex<String>>,
}

impl Gap {
    /// The next agent of the host, started once the server has stopped counting the old
    /// session as live, and waited for until it has said hello.
    fn next(self) -> Rig {
        self.server.clock.advance(Duration::from_secs(61));
        let agent = Rig::spawn(&self.server, &self.address, &self.flags, &self.machine);
        let worker = agent.try_worker().expect("the next agent's worker");
        assert_eq!(agent.next_wait(), QUICK_INTERVAL, "the hello");
        let Self {
            server,
            machine,
            flags,
            address,
        } = self;
        Rig {
            server,
            agent,
            worker,
            machine,
            flags,
            address,
        }
    }
}

/// Hands the rig's agent over: the update's request and the stop; returns what is left, and
/// what the agent printed.
fn handed_over(rig: Rig, port: &RecordingPort) -> (Gap, crate::support::Ran) {
    js_agent::lifecycle::handover::write(&node().state).expect("the handover file");
    rig.agent.signal();
    let Rig {
        server,
        agent,
        machine,
        flags,
        address,
        worker,
    } = rig;
    let ran = agent.finish();
    // The worker's thread ends when the last handle on its queue is gone, and drops its
    // runtime, which leaves each run's wiring, as it does.
    drop(worker);
    port.wait_for(5);
    let gap = Gap {
        server,
        machine,
        flags,
        address,
    };
    (gap, ran)
}

/// Asserts an update under an egress ceiling, end to end against the real server: two jobs
/// that asked for `allow-list` run; the update's request and the stop end the agent with
/// status 0 saying it handed over; no job is stopped, the wiring of each run is left (the
/// egress is told `leave` and never `release`), and each run is still alive; the next agent
/// takes over the run that still runs through the egress at the level it recorded, and settles
/// the egress once; one job ends in the gap, the other after, and the server records the first as
/// failed with its own exit and the second as succeeded, each result acknowledged once and
/// nothing started twice. It catches an agent that takes the wiring of a run down when it
/// hands over, a next agent that removes the runs it cannot tell the wiring of, and a result
/// that is lost or sent twice on the way.
#[test]
fn an_update_under_an_egress_ceiling_leaves_the_wiring_and_the_next_agent_adopts_it() {
    let _own = NODE_IN_USE.lock().unwrap_or_else(|e| e.into_inner());
    let port = Arc::new(RecordingPort::default());
    let (rig, [(first, one), (second, two)]) = running_pair("jobs_handover_egress", &port);
    assert_eq!(
        port.calls(),
        [
            "settle".to_owned(),
            format!("attach {one} allow-list 0"),
            format!("attach {two} allow-list 0"),
        ],
        "each run was wired once"
    );

    let (gap, ran) = handed_over(rig, &port);
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    assert!(
        ran.err
            .contains("js-agent: handed over: every run is left as it is to the next process"),
        "{}",
        ran.err
    );
    let mut left: Vec<String> = port.calls()[3..].to_vec();
    left.sort();
    let mut wanted = [format!("leave {one}"), format!("leave {two}")];
    wanted.sort();
    assert_eq!(left, wanted, "the wiring was left, never released");
    let log = fs::read_to_string(node().dir.path().join("calls")).expect("calls");
    assert!(
        !log.lines().any(|line| line.contains(" kill ")),
        "no job was stopped:\n{log}"
    );

    // The second job ends while no agent runs; the server's clock passes its grace.
    let node = node();
    fs::write(node.dir.path().join(format!("code-{two}")), "3").expect("an exit code");
    fs::write(node.dir.path().join(format!("go-{two}")), "go").expect("let it end");
    supervisor_ended(&two);
    let rig = gap.next();
    assert_eq!(
        port.calls()[5..],
        [format!("adopt {one} allow-list 0"), "settle".to_owned()],
        "the run that still runs is taken over at its level; the one that ended is not"
    );
    rig.until(&second, |seen| seen.1.is_some());
    assert_eq!(
        rig.view(&second),
        ("JOB_STATE_FAILED".into(), Some("RESULT_REASON_EXIT".into()))
    );
    assert_eq!(rig.view(&first).0, RUNNING);
    fs::write(node.dir.path().join(format!("go-{one}")), "go").expect("let it end");
    rig.until(&first, |seen| seen.1.is_some());
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
    let log = fs::read_to_string(node.dir.path().join("calls")).expect("calls");
    let runs = log.lines().filter(|line| line.contains(" run ")).count();
    assert_eq!(runs, 2, "nothing was created twice:\n{log}");
    assert_eq!(
        port.calls().iter().filter(|call| *call == "settle").count(),
        2,
        "each agent settled once"
    );
    assert_eq!(
        port.calls()[7..],
        [format!("release {one}")],
        "the adopted wiring is released when its run ends, and nothing else is"
    );
    let ran = rig.agent.stop();
    assert_eq!(ran.status(), code(0), "{}", ran.err);
}

/// Asserts that a run whose wiring the next agent cannot take over is not left running
/// unwired and not hidden: the egress refuses the adoption, the agent removes the run (tears
/// it down) and reports it, and the server shows its job as it shows a run the agent
/// lost, never as still running. The server's stop of each lost job reaches the agent by its
/// dispatch poll, and the agent retires both; stopped then, it drains at the pool's interval
/// until the server has acknowledged them and exits 0 with nothing left. It catches an agent
/// that adopts a run with no network, or drops it without saying so, and a drain that ends
/// with the stops of lost jobs unanswered.
#[test]
fn a_run_whose_wiring_the_next_agent_cannot_take_over_is_removed_and_reported() {
    let _own = NODE_IN_USE.lock().unwrap_or_else(|e| e.into_inner());
    let port = Arc::new(RecordingPort::default());
    let (rig, jobs) = running_pair("jobs_handover_egress_refused", &port);
    let (gap, ran) = handed_over(rig, &port);
    assert_eq!(ran.status(), code(0), "{}", ran.err);

    port.refuse_adopt.store(true, Ordering::SeqCst);
    let rig = gap.next();
    for (job, container) in &jobs {
        let adopt = format!("adopt {container} allow-list 0");
        assert!(port.calls().contains(&adopt), "{:?}", port.calls());
        rig.until(job, |seen| seen.0 != RUNNING);
        assert_eq!(rig.view(job), ("JOB_STATE_LOST".into(), None));
    }
    assert!(
        !port.calls().iter().any(|call| call.starts_with("release")),
        "a wiring that was never taken over is nothing to release: {:?}",
        port.calls()
    );
    // The server decided each loss, so it queued a stop of each job; the agent's dispatch poll
    // brings both on a thread of its own, beside the heartbeats, and the agent, which holds
    // neither job, retires each and lists it until the server acknowledges it. Both jobs were
    // lost by one listing, the last heartbeat above, and no heartbeat runs while the test waits
    // for that event, so nothing acknowledges them first and the stop below always finds both
    // owed.
    let lost: Vec<&str> = jobs.iter().map(|(job, _)| job.as_str()).collect();
    let owed = |now: &Snapshot| {
        lost.iter()
            .all(|job| now.retired_jobs.iter().any(|id| id == job))
    };
    assert!(
        rig.worker.wait_until(&owed, GUARD),
        "the stops of the lost jobs never reached the agent: {:?}",
        rig.worker.snapshot()
    );
    // The stop's drain beats while the ids are owed: it hands them over and ends once the
    // server has acknowledged them, whatever number of heartbeats that takes.
    let (ran, waits) = rig.agent.stop_draining();
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    assert!(
        !waits.is_empty() && waits.iter().all(|wait| *wait == QUICK_INTERVAL),
        "the drain beats again, at the pool's interval, while the stops are owed: {waits:?}"
    );
    assert!(
        !ran.err.contains("draining: a run or a result is left"),
        "the drain ended with nothing owed: {}",
        ran.err
    );
}
