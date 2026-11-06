//! The real agent node (`AgentKind::Real`: the lifecycle of `js-agent` over the in-memory
//! runtime) against the scripted stand-in for the server: each behaviour is shown by an
//! answer the test chose, at an exact virtual time.
//!
//! Every message takes the stand-in's `DELAY` (5 ms) and the welcome names [`INTERVAL`]
//! (1 s), so the times below follow from the script: the hello leaves at 0 and is
//! welcomed at 10, and heartbeat `n` of that session leaves at `10 + n * 1000`, reaches
//! the stand-in 5 ms later, and its answer is back 5 ms after that. A start in the answer
//! to heartbeat 1 is launched at 1 020.
//!
//! Each test gives its agent a directory of its own, under which the agent makes its
//! scratch and state roots.

use std::collections::BTreeMap;
use std::fs;
use std::time::Duration;

use js_agent_wire::{HeldState, Outcome, Phase};
use js_sim::Sim;
use js_sim_cell::{
    AgentKind, AgentSpec, Counter, Ledger, Prepare, RunId, SIM_RUN_IMAGE, StopCommand, Verdict,
};
use js_testkit::dir::TestDir;
use js_types::sched::FarmTime;
use js_types::{HostId, LeaseId, Resources};
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

/// How long a run that should outlast the test runs.
const LONG_RUN_MS: u64 = 60_000;

/// How long a run that ends inside the test runs.
const SHORT_RUN_MS: u64 = 1_500;

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
        capabilities: BTreeMap::from([("isa".to_owned(), "x86-64-v3".to_owned())]),
        labels: BTreeMap::new(),
        hello_interval: INTERVAL,
        kind: AgentKind::Real(root.path().to_path_buf()),
    }
}

/// A start of `run` in lease `1-1` that runs `millis` by itself, naming heartbeat `seq`.
fn start(run: &str, millis: u64, seq: u64) -> Value {
    json!({
        "type": "start",
        "run_id": run,
        "lease_id": "1-1",
        "image": SIM_RUN_IMAGE,
        "args": [millis.to_string()],
        "limits": { "cpu_millis": 1000, "memory_bytes": MIB, "wall_ms": 600_000 },
        "heartbeat_seq": seq,
        "valid_for_ms": 0,
    })
}

/// A reply acknowledging each run with its verdict.
fn acks(pairs: &[(&str, &str)]) -> BeatAnswer {
    let acks: Vec<Value> = pairs
        .iter()
        .map(|(run, ack)| json!({ "run_id": run, "ack": ack }))
        .collect();
    BeatAnswer::Reply(json!({ "result_acks": acks }))
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

/// Each completion in the ledger as its run and time.
fn completions(ledger: &Ledger) -> Vec<(String, u64)> {
    let each = |done: &js_sim_cell::Completion| (done.run.as_str().to_owned(), done.at.as_millis());
    ledger.completions().iter().map(each).collect()
}

fn count(sim: &Sim<Cell>, counter: Counter) -> u64 {
    agent(sim).counters().get(counter)
}

fn runs(names: &[&str]) -> Vec<String> {
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
/// and beats on the interval the welcome names. The start in the answer to heartbeat 1 is
/// launched at 1 020 and the ground truth shows it running in its lease with the
/// resources its start names; heartbeat 2 lists it running; it ends by itself at 2 520;
/// heartbeat 3 lists it exited and carries its result (lease `1-1`, succeeded, status 0);
/// the answer acknowledges it, and from heartbeat 4 nothing is held or reported. The
/// lifecycle's scratch and state roots are two directories under the agent's root.
/// Catches a real node that builds no body from the lifecycle, hands it no reply, never
/// tells the runtime a run ended, or reads the ground truth from anywhere but the runtime.
#[test]
fn a_real_agent_registers_heartbeats_and_runs_a_start() {
    let dir = root("real-agent-runs");
    let script = Script::plain()
        .commands(1, json!([start("r1", SHORT_RUN_MS, 1)]))
        .beat(3, acks(&[("r1", "ACCEPTED")]));
    let (mut sim, ledger) = cell(spec(&dir), script, INTERVAL);
    sim.run_until(ms(1_500));
    let truth = agent(&sim).truth().expect("the agent runs");
    let holds = Resources::new(1_000, MIB, 0, None).expect("r1");
    let held: Vec<(&str, LeaseId, Resources)> = truth
        .running
        .iter()
        .map(|(run, running)| (run.as_str(), running.lease, running.resources.clone()))
        .collect();
    assert_eq!(held, [("r1", LeaseId::new(1, 1), holds)]);
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
    assert_eq!(server.beat_times(), [1_015, 2_015, 3_015, 4_015, 5_015]);
    let none = runs(&[]);
    let r1 = runs(&["r1"]);
    let carried = [&none, &none, &r1, &none, &none].map(Clone::clone);
    assert_eq!(server.results(), carried);
    let beats = server.beats();
    assert!(beats[0].1.held.is_empty());
    assert_eq!(beats[1].1.held[0].state, HeldState::Running);
    assert_eq!(beats[2].1.held[0].state, HeldState::ExitedUnacked);
    assert!(beats[3].1.held.is_empty());
    let result = &beats[2].1.results[0];
    assert_eq!(result.lease_id, "1-1");
    assert_eq!(result.outcome, Outcome::Succeeded);
    assert_eq!(result.exit_code, Some(0));

    assert_eq!(launches(&ledger), [("r1".to_owned(), 1_020)]);
    assert_eq!(completions(&ledger), [("r1".to_owned(), 2_520)]);
    let truth = agent(&sim).truth().expect("the agent runs");
    assert!(truth.running.is_empty());
    let ended = BTreeMap::from([(RunId::new("r1"), ms(2_520))]);
    assert_eq!(truth.completed, ended);
    let accepted = BTreeMap::from([(RunId::new("r1"), Verdict::Accepted)]);
    assert_eq!(truth.acknowledged, accepted);
    assert_eq!(count(&sim, Counter::ResultResent), 0);
}

/// `r1` ends at 2 520 and its result rides in heartbeat 3, whose answer is lost; in
/// heartbeat 4, answered with a verdict this version cannot read and an acknowledgement of
/// a run the agent does not hold; and in heartbeat 5, answered `STALE`, which ends it.
/// Heartbeat 6 carries nothing. The two sends after the first are counted, and the ground
/// truth records the verdict that ended it. Catches a real node that drops a result whose
/// acknowledgement was lost, records a verdict the lifecycle did not act on, or records
/// one for a run it does not hold.
#[test]
fn a_real_agent_resends_an_unacknowledged_result_until_acknowledged() {
    let dir = root("real-agent-resends");
    let script = Script::plain()
        .commands(1, json!([start("r1", SHORT_RUN_MS, 1)]))
        .beat(3, BeatAnswer::Silent)
        .beat(4, acks(&[("r1", "SOMETHING_NEWER"), ("zz", "ACCEPTED")]))
        .beat(5, acks(&[("r1", "STALE")]));
    let (sim, _) = run(&dir, script, 6_500);
    let none = runs(&[]);
    let r1 = runs(&["r1"]);
    let carried = [&none, &none, &r1, &r1, &r1, &none].map(Clone::clone);
    assert_eq!(stand_in(&sim).results(), carried);
    assert_eq!(count(&sim, Counter::ResultResent), 2);
    let truth = agent(&sim).truth().expect("the agent runs");
    let stale = BTreeMap::from([(RunId::new("r1"), Verdict::Stale)]);
    assert_eq!(truth.acknowledged, stale);
}

/// The start of `r1` (3 s) arrives in the answers to heartbeats 1 and 2: the real agent
/// launches it once, at 1 020, and the node counts the repeat. A stop in the answer to
/// heartbeat 3 ends it; the real agent reports it killed; the start that arrives after the
/// stop is counted and runs nothing, and `r1`'s own end at 4 020 finds it no longer
/// running, so it is never completed. Catches a real node that launches what the
/// lifecycle did not, misses a repeated or stale start, or completes a stopped run.
#[test]
fn a_real_agent_ignores_a_repeated_start_for_a_run_it_holds() {
    let dir = root("real-agent-repeat");
    let script = Script::plain()
        .commands(1, json!([start("r1", 3_000, 1)]))
        .commands(2, json!([start("r1", 3_000, 1)]))
        .commands(3, json!([{ "type": "stop", "run_id": "r1" }]))
        .commands(4, json!([start("r1", 3_000, 4)]));
    let (sim, ledger) = run(&dir, script, 5_500);
    assert_eq!(launches(&ledger), [("r1".to_owned(), 1_020)]);
    assert_eq!(count(&sim, Counter::StartRepeated), 1);
    assert_eq!(count(&sim, Counter::StartAfterStop), 1);
    assert!(completions(&ledger).is_empty());
    let beats = stand_in(&sim).beats();
    let reported: Vec<Outcome> = beats
        .iter()
        .flat_map(|(_, beat)| beat.results.iter())
        .map(|result| result.outcome)
        .collect();
    assert!(!reported.is_empty());
    assert!(reported.iter().all(|outcome| *outcome == Outcome::Killed));
    let truth = agent(&sim).truth().expect("the agent runs");
    assert!(truth.running.is_empty());
    assert!(truth.completed.is_empty());
}

/// Two runs launch at 1 020. The process is killed at 2 012 and replaced at 4 500. The
/// replacement has new roots (the killed process's are gone), holds nothing, says hello
/// again at the node's next tick (5 010) with nothing held, is welcomed into a new session
/// and beats with nothing to report; the end timers the dead process armed do nothing.
/// The cell's ledger still shows both launches. Catches a real replacement that inherits
/// its predecessor's runtime, roots or timers, and a ledger kept in the process.
#[test]
fn a_real_agent_killed_and_replaced_lists_nothing_and_says_hello_again() {
    let dir = root("real-agent-replaced");
    let script = Script::plain().commands(1, json!([start("a", 2_000, 1), start("b", 9_000, 1)]));
    let (mut sim, ledger) = cell(spec(&dir), script, INTERVAL);
    sim.run_until(ms(2_012));
    assert_eq!(agent(&sim).truth().expect("alive").running.len(), 2);
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
    assert!(fresh.running.is_empty());
    let second = entries(&dir);
    assert_eq!(second.len(), 2);
    assert!(
        second.iter().all(|name| !first.contains(name)),
        "{first:?} {second:?}"
    );
    sim.run_until(ms(12_000));

    let server = stand_in(&sim);
    assert_eq!(server.hello_times(), [5, 5_015]);
    assert!(server.hellos()[1].held.is_empty());
    let after: Vec<&js_agent_wire::Heartbeat> = server
        .beats()
        .into_iter()
        .filter(|(at, _)| *at > 5_015)
        .map(|(_, beat)| beat)
        .collect();
    assert_eq!(after.len(), 6);
    for beat in after {
        assert_eq!(beat.epoch, 2);
        assert!(beat.held.is_empty());
        assert!(beat.results.is_empty());
    }
    let truth = agent(&sim).truth().expect("replaced");
    assert!(truth.running.is_empty());
    assert!(truth.completed.is_empty());
    let both = [("a".to_owned(), 1_020), ("b".to_owned(), 1_020)];
    assert_eq!(launches(&ledger), both);
    assert!(completions(&ledger).is_empty());
}

/// A 409 `unknown-host` to heartbeat 2, after heartbeat 1 was accepted, ends the session:
/// the real agent says hello again at once (it reaches the stand-in at 2 025), listing the
/// run it holds, and beats in the new session one interval after its welcome. Catches a
/// real node that keeps beating into a session the server lost, or a hello that does not
/// list what the lifecycle holds.
#[test]
fn a_real_agent_follows_unknown_host_with_a_hello() {
    let dir = root("real-agent-unknown-host");
    let refuse = BeatAnswer::Refuse {
        status: 409,
        code: "unknown-host",
    };
    let script = Script::plain()
        .commands(1, json!([start("r1", LONG_RUN_MS, 1)]))
        .beat(2, refuse);
    let (sim, ledger) = run(&dir, script, 4_500);
    let server = stand_in(&sim);
    assert_eq!(server.hello_times(), [5, 2_025]);
    assert_eq!(server.beat_times(), [1_015, 2_015, 3_035, 4_035]);
    let sessions: Vec<(u64, String)> = server
        .beats()
        .iter()
        .map(|(_, beat)| (beat.epoch, beat.session.clone()))
        .collect();
    let session = |epoch: u64| (epoch, format!("{SESSION_PREFIX}{epoch}"));
    assert_eq!(sessions, [session(1), session(1), session(2), session(2)]);
    let held = &server.hellos()[1].held;
    assert_eq!(held.len(), 1);
    assert_eq!(
        (held[0].run_id.as_str(), held[0].state),
        ("r1", HeldState::Running)
    );
    assert_eq!(launches(&ledger), [("r1".to_owned(), 1_020)]);
}

/// The answer to heartbeat 1 carries a start whose lease the cell's server would never
/// write, a command this version does not know, and a good start: the real agent is handed
/// the good start alone and launches it, and the node counts the reply once as invalid.
/// A reply that does not decode is counted and changes nothing. Catches a real node that
/// lets the lifecycle run a start the cell cannot account for (the ground truth would miss
/// it), or that stops at a command it does not know.
#[test]
fn a_real_agent_is_handed_only_the_starts_the_cell_can_account_for() {
    let dir = root("real-agent-unaccounted");
    let mut odd = start("odd", LONG_RUN_MS, 1);
    odd["lease_id"] = json!("lease");
    let newer = json!({ "type": "reboot", "run_id": "r3" });
    let script = Script::plain()
        .commands(1, json!([odd, newer, start("r1", LONG_RUN_MS, 1)]))
        .beat(2, BeatAnswer::Raw("not json"));
    let (sim, ledger) = run(&dir, script, 3_500);
    assert_eq!(launches(&ledger), [("r1".to_owned(), 1_020)]);
    assert_eq!(count(&sim, Counter::ReplyInvalid), 2);
    let truth = agent(&sim).truth().expect("the agent runs");
    let running: Vec<&str> = truth.running.keys().map(RunId::as_str).collect();
    assert_eq!(running, ["r1"]);
    let last = stand_in(&sim).beats().pop().expect("a heartbeat").1;
    let held: Vec<&str> = last.held.iter().map(|run| run.run_id.as_str()).collect();
    assert_eq!(held, ["r1"]);
}

/// A real agent, silent on a prepare, goes through the whole-host handshake of the
/// session as the fake does: answer 1 names lease `1-1` and carries a start of it, which
/// is written into the ledger and not handed to the lifecycle (a cancel beside it is
/// handed over) (heartbeat 2 says
/// `preparing`, and nothing is launched or counted invalid); answer 2 names no lease
/// (heartbeat 3 says `restoring`); answer 3 carries a start, which the serving agent's
/// lifecycle launches at 3 020; answer 4 stops it, and the ledger holds the stop of the
/// running run. Catches a real agent that launches a start while it prepares, one whose
/// withheld start is counted as one the cell cannot account for, and a stop of a real
/// agent's run left out of the ledger.
#[test]
fn a_real_agent_launches_no_start_while_it_prepares_and_its_stops_reach_the_ledger() {
    let root = root("real-exclusive");
    let script = Script::plain()
        .beat(
            1,
            BeatAnswer::Reply(json!({
                "exclusive": "1-1",
                "commands": [start("r1", LONG_RUN_MS, 1), { "type": "cancel", "run_id": "r8" }],
            })),
        )
        .commands(3, json!([start("r2", LONG_RUN_MS, 3)]))
        .commands(4, json!([{ "type": "stop", "run_id": "r2" }]));
    let (mut sim, ledger) = cell(spec(&root), script, INTERVAL);
    agent_mut(&mut sim).set_prepare(Prepare::Silent);
    sim.run_until(ms(4_500));
    let phases: Vec<Phase> = stand_in(&sim)
        .beats()
        .iter()
        .map(|(_, b)| b.phase)
        .collect();
    let wanted = [
        Phase::Serving,
        Phase::Preparing,
        Phase::Restoring,
        Phase::Serving,
    ];
    assert_eq!(phases, wanted);
    let started: Vec<RunId> = ledger.starts().into_iter().map(|s| s.run).collect();
    assert_eq!(started, [RunId::new("r1"), RunId::new("r2")]);
    assert_eq!(launches(&ledger), [("r2".to_owned(), 3_020)]);
    assert_eq!(count(&sim, Counter::ReplyInvalid), 0);
    let stops = ledger.stops();
    assert_eq!(stops.len(), 1);
    assert_eq!(stops[0].run, RunId::new("r2"));
    assert_eq!(stops[0].command, StopCommand::Stop);
    assert_eq!(stops[0].at, ms(4_020));
}
