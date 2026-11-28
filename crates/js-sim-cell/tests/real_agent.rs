//! The real agent node (`AgentKind::Real`: the lifecycle of `js-agent` over the in-memory
//! runtime) against the scripted stand-in for the server: each behaviour is shown by an
//! answer the test chose, at an exact virtual time.
//!
//! Every message takes the stand-in's `DELAY` (5 ms) and the welcome names [`INTERVAL`]
//! (1 s), so the times below follow from the script: the hello leaves at 0 and is
//! welcomed at 10, and the agent has one call in flight at a time and sends its next
//! heartbeat an interval after the answer to the last, so heartbeat `n` of that session
//! leaves at `n * 1010`, reaches the stand-in 5 ms later, and its answer is read 5 ms after
//! that. A job start in the answer to heartbeat 1 is launched at 1 020.
//!
//! Each test gives its agent a directory of its own, under which the agent makes its
//! scratch and state roots.

use std::collections::BTreeMap;
use std::fs;
use std::time::Duration;

use js_agent_wire::{HeldState, Outcome, PER_JOB_CAPABILITY};
use js_sim::Sim;
use js_sim_cell::{AgentKind, AgentSpec, Counter, Ledger, SIM_RUN_IMAGE};
use js_testkit::dir::TestDir;
use js_types::sched::FarmTime;
use js_types::{HostId, Resources};
use serde_json::{Value, json};

#[path = "agent/standin.rs"]
#[allow(
    dead_code,
    reason = "the stand-in is shared with tests/agent.rs, which scripts hellos and late answers this binary does not"
)]
mod standin;

use standin::{BeatAnswer, Cell, POOL, SESSION_PREFIX, Script, agent, agent_mut, cell, stand_in};

/// The heartbeat interval the stand-in's welcome names, and the agent's hello interval.
const INTERVAL: Duration = Duration::from_millis(1_000);

/// The join token of every agent here.
const JOIN: &str = "join-token-sim-0123456789-abcdefghij";

/// How long a job that should outlast the test runs.
const LONG_RUN_MS: u64 = 60_000;

/// How long a job that ends inside the test runs.
const SHORT_RUN_MS: u64 = 1_500;

/// The allocation every job start here names.
const ALLOCATION: &str = "0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a";

/// The jobs the tests start.
const JOB0: &str = "sim-job-0";
const JOB1: &str = "sim-job-1";
const JOB2: &str = "sim-job-2";

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;

fn ms(n: u64) -> FarmTime {
    FarmTime::from_millis(n)
}

/// A directory of the test's own for its agent's roots.
fn root(name: &str) -> TestDir {
    TestDir::under(env!("CARGO_TARGET_TMPDIR"), name)
}

/// A real agent on `h000`, its roots under `root`: 8 cores, 16 GiB and one GPU.
fn spec(root: &TestDir) -> AgentSpec {
    AgentSpec {
        host: HostId::new("h000").expect("host id"),
        pool: POOL.to_owned(),
        join_token: JOIN.to_owned(),
        capacity: Resources::new(8_000, 16 * GIB, 1, Some("nvidia-a100".to_owned()))
            .expect("capacity"),
        capabilities: BTreeMap::from([
            ("isa".to_owned(), "x86-64-v3".to_owned()),
            (PER_JOB_CAPABILITY.to_owned(), "true".to_owned()),
        ]),
        labels: BTreeMap::new(),
        hello_interval: INTERVAL,
        kind: AgentKind::Real(root.path().to_path_buf()),
        instance: None,
        worker_lag: std::time::Duration::ZERO,
    }
}

/// A start of `job` under [`ALLOCATION`] that runs `millis` by itself.
fn start(job: &str, millis: u64) -> Value {
    json!({
        "type": "job_start",
        "job_id": job,
        "allocation_id": ALLOCATION,
        "image": SIM_RUN_IMAGE,
        "command": [millis.to_string()],
        "booked": { "cpu_millis": 1000, "memory_bytes": MIB },
        "wall_ms": 600_000,
    })
}

/// A reply acknowledging each job with its verdict.
fn acks(pairs: &[(&str, &str)]) -> BeatAnswer {
    let acks: Vec<Value> = pairs
        .iter()
        .map(|(job, ack)| json!({ "job_id": job, "ack": ack }))
        .collect();
    BeatAnswer::Reply(json!({ "job_acks": acks }))
}

/// The cell `script` plays against the real agent of `root`, run to `until` ms.
fn run(root: &TestDir, script: Script, until: u64) -> (Sim<Cell>, Ledger) {
    let (mut sim, ledger) = cell(spec(root), script, INTERVAL);
    sim.run_until(ms(until));
    (sim, ledger)
}

/// Each launch in the ledger as its run and time.
fn launches(ledger: &Ledger) -> Vec<(String, u64)> {
    let each = |launch: &js_sim_cell::observe::Launch| {
        assert_eq!(launch.host.as_str(), "h000");
        (launch.run.as_str().to_owned(), launch.at.as_millis())
    };
    ledger.launches().iter().map(each).collect()
}

/// The name a launch of `job` has in the ledger.
fn named(job: &str) -> String {
    format!("{job}/{ALLOCATION}")
}

fn count(sim: &Sim<Cell>, counter: Counter) -> u64 {
    agent(sim).counters().get(counter)
}

fn jobs(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

/// The names of the entries of `dir`, sorted.
fn entries(dir: &TestDir) -> Vec<String> {
    let names = fs::read_dir(dir.path()).expect("the root is readable");
    let mut names: Vec<String> = names
        .map(|entry| {
            entry
                .expect("an entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

/// The real agent says hello at once with the host's shape and nothing held, is welcomed,
/// and beats on the interval the welcome names. The job start in the answer to heartbeat 1
/// is launched at 1 020 and the ground truth shows it running in its allocation with the
/// resources its start books; heartbeat 2 lists it running; it ends by itself at 2 520;
/// heartbeat 3 lists it exited and carries its result (allocation `0a0a...`, succeeded,
/// status 0); the answer acknowledges it, and from heartbeat 4 nothing is held or reported.
/// No heartbeat lists a run or sends a run result. The lifecycle's scratch and state roots
/// are two directories under the agent's root. Catches a real node that builds no body from
/// the lifecycle, hands it no reply, never tells the runtime a job ended, or reads the
/// ground truth from anywhere but the runtime.
#[test]
fn a_real_agent_registers_heartbeats_and_runs_a_start() {
    let dir = root("real-agent-runs");
    let script = Script::plain()
        .commands(1, json!([start(JOB0, SHORT_RUN_MS)]))
        .beat(3, acks(&[(JOB0, "ACCEPTED")]));
    let (mut sim, ledger) = cell(spec(&dir), script, INTERVAL);
    sim.run_until(ms(1_500));
    let truth = agent(&sim).truth().expect("the agent runs");
    let holds = Resources::new(1_000, MIB, 0, None).expect("job");
    let held: Vec<(String, String, Resources)> = truth
        .jobs
        .iter()
        .map(|(job, running)| {
            let allocation = running.allocation.to_string();
            (job.to_string(), allocation, running.resources.clone())
        })
        .collect();
    assert_eq!(held, [(JOB0.to_owned(), ALLOCATION.to_owned(), holds)]);
    assert_eq!(truth.capacity, spec(&dir).capacity);
    let roots = entries(&dir);
    assert_eq!(roots.len(), 2, "{roots:?}");
    assert!(roots[0].starts_with("real-h000-scratch-"), "{roots:?}");
    assert!(roots[1].starts_with("real-h000-state-"), "{roots:?}");
    sim.run_until(ms(5_500));

    let server = stand_in(&sim);
    assert_eq!(server.hello_times(), [5]);
    let hello = server.hellos()[0];
    assert!(hello.held.is_empty());
    assert_eq!(hello.pool, POOL);
    assert_eq!(hello.shape.cpu_millis, 8_000);
    assert_eq!(hello.shape.gpus, 1);
    assert!(server.received.iter().all(|call| call.bearer == JOIN));
    assert_eq!(server.beat_times(), [1_015, 2_025, 3_035, 4_045, 5_055]);
    let none = jobs(&[]);
    let one = jobs(&[JOB0]);
    let carried = [&none, &none, &one, &none, &none].map(Clone::clone);
    assert_eq!(server.results(), carried);
    let beats = server.beats();
    assert!(beats[0].1.jobs.is_empty());
    assert_eq!(beats[1].1.jobs[0].state, HeldState::Running);
    assert_eq!(beats[2].1.jobs[0].state, HeldState::ExitedUnacked);
    assert!(beats[3].1.jobs.is_empty());
    let result = beats[2].1.jobs[0].result.as_ref().expect("a result");
    assert_eq!(result.allocation_id, ALLOCATION);
    assert_eq!(result.outcome, Outcome::Succeeded);
    assert_eq!(result.exit_code, Some(0));
    for (_, beat) in &beats {
        assert!(beat.held.is_empty());
        assert!(beat.results.is_empty());
    }

    assert_eq!(launches(&ledger), [(named(JOB0), 1_020)]);
    let truth = agent(&sim).truth().expect("the agent runs");
    assert!(truth.jobs.is_empty());
    assert_eq!(count(&sim, Counter::ResultResent), 0);
}

/// The job ends at 2 520 and its result rides in heartbeat 3, whose answer is lost; in
/// heartbeat 4, answered with a verdict this version cannot read and an acknowledgement of a
/// job the agent does not hold; and in heartbeat 5, answered `STALE`, which ends it.
/// Heartbeat 6 carries nothing. The two sends after the first are counted. Catches a real
/// node that drops a result whose acknowledgement was lost, or one that ends a result on a
/// verdict it cannot read or on an acknowledgement of a job it does not hold.
#[test]
fn a_real_agent_resends_an_unacknowledged_result_until_acknowledged() {
    let dir = root("real-agent-resends");
    let script = Script::plain()
        .commands(1, json!([start(JOB0, SHORT_RUN_MS)]))
        .beat(3, BeatAnswer::Silent)
        .beat(4, acks(&[(JOB0, "SOMETHING_NEWER"), (JOB2, "ACCEPTED")]))
        .beat(5, acks(&[(JOB0, "STALE")]));
    let (sim, _) = run(&dir, script, 17_000);
    let none = jobs(&[]);
    let one = jobs(&[JOB0]);
    let carried = [&none, &none, &one, &one, &one, &none].map(Clone::clone);
    assert_eq!(stand_in(&sim).results(), carried);
    assert_eq!(count(&sim, Counter::ResultResent), 2);
}

/// The start of `sim-job-0` (3 s) arrives in the answers to heartbeats 1 and 2: the real
/// agent launches it once, at 1 020, and the node counts the repeat. A stop in the answer to
/// heartbeat 3 ends it; the real agent reports it killed; the start that arrives after the
/// stop is counted and runs nothing, and the job's own end at 4 020 finds it no longer
/// running. Catches a real node that launches what the lifecycle did not, misses a repeated
/// or stale start, or reports a stopped job as succeeded.
#[test]
fn a_real_agent_ignores_a_repeated_start_for_a_job_it_holds() {
    let dir = root("real-agent-repeat");
    let script = Script::plain()
        .commands(1, json!([start(JOB0, 3_000)]))
        .commands(2, json!([start(JOB0, 3_000)]))
        .commands(3, json!([{ "type": "job_stop", "job_id": JOB0 }]))
        .commands(4, json!([start(JOB0, 3_000)]));
    let (sim, ledger) = run(&dir, script, 5_500);
    assert_eq!(launches(&ledger), [(named(JOB0), 1_020)]);
    assert_eq!(count(&sim, Counter::StartRepeated), 1);
    assert_eq!(count(&sim, Counter::StartAfterStop), 1);
    let beats = stand_in(&sim).beats();
    let reported: Vec<Outcome> = beats
        .iter()
        .flat_map(|(_, beat)| beat.jobs.iter())
        .filter_map(|job| job.result.as_ref())
        .map(|result| result.outcome)
        .collect();
    assert!(!reported.is_empty());
    assert!(reported.iter().all(|outcome| *outcome == Outcome::Killed));
    let truth = agent(&sim).truth().expect("the agent runs");
    assert!(truth.jobs.is_empty());
}

/// Two jobs launch at 1 020. The process is killed at 2 012 and replaced at 4 500. The
/// replacement has new roots (the killed process's are gone), holds nothing, says hello
/// again at the node's next tick (5 020) with nothing held, is welcomed into a new session
/// and beats with nothing to report; the end timers the dead process armed do nothing.
/// The cell's ledger still shows both launches. Catches a real replacement that inherits
/// its predecessor's runtime, roots or timers, and a ledger kept in the process.
#[test]
fn a_real_agent_killed_and_replaced_lists_nothing_and_says_hello_again() {
    let dir = root("real-agent-replaced");
    let script = Script::plain().commands(1, json!([start(JOB0, 2_000), start(JOB1, 9_000)]));
    let (mut sim, ledger) = cell(spec(&dir), script, INTERVAL);
    sim.run_until(ms(2_012));
    assert_eq!(agent(&sim).truth().expect("alive").jobs.len(), 2);
    let first = entries(&dir);
    agent_mut(&mut sim).kill();
    assert!(agent(&sim).truth().is_none());
    assert!(
        entries(&dir).is_empty(),
        "the killed process's roots are removed"
    );
    sim.run_until(ms(4_500));
    agent_mut(&mut sim).replace();
    let fresh = agent(&sim).truth().expect("replaced");
    assert!(fresh.jobs.is_empty());
    let second = entries(&dir);
    assert_eq!(second.len(), 2);
    assert!(
        second.iter().all(|name| !first.contains(name)),
        "{first:?} {second:?}"
    );
    sim.run_until(ms(12_000));

    let server = stand_in(&sim);
    assert_eq!(server.hello_times(), [5, 5_025]);
    assert!(server.hellos()[1].held.is_empty());
    let after: Vec<&js_agent_wire::Heartbeat> = server
        .beats()
        .into_iter()
        .filter(|(at, _)| *at > 5_025)
        .map(|(_, beat)| beat)
        .collect();
    assert_eq!(after.len(), 6);
    for beat in after {
        assert_eq!(beat.epoch, 2);
        assert!(beat.held.is_empty());
        assert!(beat.results.is_empty());
        assert!(beat.jobs.is_empty());
    }
    let truth = agent(&sim).truth().expect("replaced");
    assert!(truth.jobs.is_empty());
    let both = [(named(JOB0), 1_020), (named(JOB1), 1_020)];
    assert_eq!(launches(&ledger), both);
}

/// A 409 `unknown-host` to heartbeat 2, after heartbeat 1 was accepted, ends the session:
/// the real agent says hello again at once (it reaches the stand-in at 2 035) with nothing
/// held (a hello lists no run, and no job), and beats in the new session one interval after
/// its welcome, listing the job it holds. Catches a real node that keeps beating into a
/// session the server lost, or a heartbeat that does not list what the lifecycle holds.
#[test]
fn a_real_agent_follows_unknown_host_with_a_hello() {
    let dir = root("real-agent-unknown-host");
    let refuse = BeatAnswer::Refuse {
        status: 409,
        code: "unknown-host",
    };
    let script = Script::plain()
        .commands(1, json!([start(JOB0, LONG_RUN_MS)]))
        .beat(2, refuse);
    let (sim, ledger) = run(&dir, script, 4_500);
    let server = stand_in(&sim);
    assert_eq!(server.hello_times(), [5, 2_035]);
    assert_eq!(server.beat_times(), [1_015, 2_025, 3_045, 4_055]);
    let sessions: Vec<(u64, String)> = server
        .beats()
        .iter()
        .map(|(_, beat)| (beat.epoch, beat.session.clone()))
        .collect();
    let session = |epoch: u64| (epoch, format!("{SESSION_PREFIX}{epoch}"));
    assert_eq!(sessions, [session(1), session(1), session(2), session(2)]);
    assert!(server.hellos()[1].held.is_empty());
    let listed = &server.beats()[2].1.jobs;
    assert_eq!(listed.len(), 1);
    assert_eq!(
        (listed[0].job_id.as_str(), listed[0].state),
        (JOB0, HeldState::Running)
    );
    assert_eq!(launches(&ledger), [(named(JOB0), 1_020)]);
}

/// The answer to heartbeat 1 carries a start whose allocation the cell's server would never
/// write, a command this version does not know, and a good start: the real agent is handed
/// the good start alone and launches it, and the node counts the reply once as invalid.
/// A reply that does not decode is counted and changes nothing. Catches a real node that
/// lets the lifecycle run a start the cell cannot account for (the ground truth would miss
/// it), or that stops at a command it does not know.
#[test]
fn a_real_agent_is_handed_only_the_starts_the_cell_can_account_for() {
    let dir = root("real-agent-unaccounted");
    let mut odd = start(JOB2, LONG_RUN_MS);
    odd["allocation_id"] = json!("allocation");
    let newer = json!({ "type": "reboot", "job_id": "sim-job-3" });
    let script = Script::plain()
        .commands(1, json!([odd, newer, start(JOB0, LONG_RUN_MS)]))
        .beat(2, BeatAnswer::Raw("not json"));
    let (sim, ledger) = run(&dir, script, 3_500);
    assert_eq!(launches(&ledger), [(named(JOB0), 1_020)]);
    assert_eq!(count(&sim, Counter::ReplyInvalid), 2);
    let truth = agent(&sim).truth().expect("the agent runs");
    let running: Vec<String> = truth.jobs.keys().map(ToString::to_string).collect();
    assert_eq!(running, [JOB0]);
    let last = stand_in(&sim).beats().pop().expect("a heartbeat").1;
    let held: Vec<&str> = last.jobs.iter().map(|job| job.job_id.as_str()).collect();
    assert_eq!(held, [JOB0]);
}

/// The answer to heartbeat 1 carries a stop of a job the agent does not hold: the real
/// agent takes it without complaint (the reply is not counted invalid, and nothing is
/// launched), and the heartbeats go on. Catches a real node that lets a job stop in a reply
/// break the session.
#[test]
fn a_real_agent_takes_a_job_stop_for_a_job_it_does_not_hold() {
    let dir = root("real-agent-job-stop");
    let stop = json!({ "type": "job_stop", "job_id": JOB1 });
    let script = Script::plain().commands(1, json!([stop]));
    let (sim, ledger) = run(&dir, script, 3_500);
    assert!(launches(&ledger).is_empty());
    assert_eq!(count(&sim, Counter::ReplyInvalid), 0);
    assert_eq!(stand_in(&sim).beat_times(), [1_015, 2_025, 3_035]);
}
