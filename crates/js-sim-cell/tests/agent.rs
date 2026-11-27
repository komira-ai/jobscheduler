//! The agent node against a scripted stand-in for the server: each behaviour is shown by
//! an answer the test chose, at an exact virtual time.
//!
//! Every message takes the stand-in's `DELAY` (5 ms) and the welcome names [`INTERVAL`]
//! (1 s), so the times below follow from the script: the hello leaves at 0 and is
//! welcomed at 10, and heartbeat `n` of that session leaves at `10 + n * 1000`, reaches
//! the stand-in 5 ms later, and its answer is back 5 ms after that.

use std::collections::BTreeMap;
use std::time::Duration;

use js_agent_wire::{HeldState, PER_JOB_CAPABILITY, RETRY_AFTER_CAP};
use js_conformance::agent::Defect;
use js_sim::{Event, Node, NodeId, NodeInput, Sim};
use js_sim_cell::bus::{SERVER, agent_name};
use js_sim_cell::{
    AgentKind, AgentNode, AgentSpec, CallId, Checker, Counter, Invariant, Ledger, Msg,
    Observations, Route, SIM_RUN_IMAGE, Touched,
};
use js_types::sched::{FarmTime, StateMachine};
use js_types::{HostId, Resources};
use serde_json::{Value, json};

#[path = "agent/standin.rs"]
mod standin;
// The whole-host handshake, a module of this test binary.
#[path = "agent/exclusive.rs"]
mod exclusive;

use standin::{
    BeatAnswer, Cell, HelloAnswer, POOL, SESSION_PREFIX, Script, agent, agent_mut, cell, stand_in,
};

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

/// How long after the first the stand-in sends a welcome a second time.
const SECOND_WELCOME_AFTER: Duration = Duration::from_millis(500);

/// How long the stand-in holds back a late answer.
const HELD_BACK: Duration = Duration::from_millis(2_500);

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;

fn ms(n: u64) -> FarmTime {
    FarmTime::from_millis(n)
}

/// A correct agent on `h000`: 8 cores, 16 GiB and one GPU.
fn spec() -> AgentSpec {
    AgentSpec {
        host: HostId::new("h000").expect("host id"),
        pool: POOL.to_owned(),
        join_token: JOIN.to_owned(),
        capacity: Resources::new(8_000, 16 * GIB, 1, Some("nvidia-a100".to_owned()))
            .expect("capacity"),
        capabilities: BTreeMap::from([("isa".to_owned(), "x86-64-v3".to_owned())]),
        labels: BTreeMap::from([("rack".to_owned(), "r1".to_owned())]),
        hello_interval: INTERVAL,
        kind: AgentKind::Fake(None),
        instance: None,
        worker_lag: std::time::Duration::ZERO,
    }
}

/// The agent of [`spec`] that speaks the per-job messages.
fn job_spec() -> AgentSpec {
    let mut spec = spec();
    spec.capabilities
        .insert(PER_JOB_CAPABILITY.to_owned(), "true".to_owned());
    spec
}

/// The agent of [`job_spec`], lacking `defect`.
fn broken(defect: Defect) -> AgentSpec {
    AgentSpec {
        kind: AgentKind::Fake(Some(defect)),
        ..job_spec()
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

/// A stop of `job`.
fn stop(job: &str) -> Value {
    json!({ "type": "job_stop", "job_id": job })
}

/// The name a launch of `job` has in the ledger.
fn named(job: &str) -> String {
    format!("{job}/{ALLOCATION}")
}

/// A reply acknowledging each job with its verdict.
fn acks(pairs: &[(&str, &str)]) -> BeatAnswer {
    let acks: Vec<Value> = pairs
        .iter()
        .map(|(job, ack)| json!({ "job_id": job, "ack": ack }))
        .collect();
    BeatAnswer::Reply(json!({ "job_acks": acks }))
}

/// The cell `script` plays against `spec`, run to `until` ms.
fn run(spec: AgentSpec, script: Script, until: u64) -> (Sim<Cell>, Ledger) {
    let (mut sim, ledger) = cell(spec, script, INTERVAL);
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

/// The invariants the cell's checker finds broken over the ledger alone.
fn ledger_violations(ledger: &Ledger, at: u64) -> Vec<Invariant> {
    let mut observations = Observations::quiet().at(ms(at));
    for launch in ledger.launches() {
        observations = observations.with_launch(launch.run, launch.host, launch.at);
    }
    let found = Checker::new().step(&observations, &Touched::nothing());
    found
        .into_iter()
        .map(|violation| violation.invariant)
        .collect()
}

fn count(sim: &Sim<Cell>, counter: Counter) -> u64 {
    agent(sim).counters().get(counter)
}

fn jobs(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

/// The stand-in drops the first two hellos; the third, an interval after the second, is
/// welcomed, and the heartbeats follow one interval apart from the welcome. Every call
/// carries the join token, though the welcome offered a host credential. Catches an agent
/// that gives up after a lost hello, hellos off the interval, beats off the interval, or
/// keeps a credential it did not ask for.
#[test]
fn a_hello_is_resent_until_welcomed_and_heartbeats_follow_on_the_interval() {
    let script = Script::plain()
        .hello(1, HelloAnswer::Silent)
        .hello(2, HelloAnswer::Silent);
    let (sim, _) = run(spec(), script, 6_500);
    let server = stand_in(&sim);
    assert_eq!(server.hello_times(), [5, 1_005, 2_005]);
    assert_eq!(server.beat_times(), [3_015, 4_025, 5_035, 6_045]);
    for received in &server.received {
        assert_eq!(received.bearer, JOIN);
    }
    for hello in server.hellos() {
        assert!(!hello.accepts_host_credential);
        assert_eq!(hello.host_id, "h000");
        assert_eq!(hello.pool, POOL);
    }
    for (_, beat) in server.beats() {
        assert_eq!(
            (beat.epoch, beat.session.as_str()),
            (1, "standin-session-1")
        );
    }
}

/// A 429 with a `Retry-After` is followed by one hello exactly that wait later, and none
/// before; a wait above the wire's cap is cut to the cap. Catches an agent that hammers a
/// server that asked it to wait, one that gives up, and one that obeys a wait without
/// bound.
#[test]
fn a_refused_hello_is_retried_after_the_wait_it_names() {
    const WAIT: Duration = Duration::from_millis(3_500);
    const TOO_LONG: Duration = Duration::from_secs(1_000);
    let refuse = |wait| HelloAnswer::Refuse {
        status: 429,
        code: "rate-limited",
        retry_after: Some(wait),
    };
    let script = Script::plain()
        .hello(1, refuse(WAIT))
        .hello(2, refuse(TOO_LONG));
    let (sim, _) = run(spec(), script, 126_000);
    let cap = u64::try_from(RETRY_AFTER_CAP.as_millis()).expect("the cap fits");
    let first_answer: u64 = 10;
    let second = first_answer + 3_500;
    let third = second + 10 + cap;
    let server = stand_in(&sim);
    assert_eq!(server.hello_times(), [5, second + 5, third + 5]);
    assert_eq!(server.beat_times(), [third + 1_015, third + 2_025]);
}

/// A refusal other than 429 (here 403 `pool-full`), and a 429 that names no wait, are
/// followed by the next hello one interval later. Catches an agent that gives up on a
/// refusal it does not read as final, or retries at once.
#[test]
fn a_hello_refused_without_a_wait_is_retried_on_the_interval() {
    let script = Script::plain()
        .hello(
            1,
            HelloAnswer::Refuse {
                status: 403,
                code: "pool-full",
                retry_after: None,
            },
        )
        .hello(
            2,
            HelloAnswer::Refuse {
                status: 429,
                code: "rate-limited",
                retry_after: None,
            },
        );
    let (sim, _) = run(spec(), script, 3_500);
    let server = stand_in(&sim);
    assert_eq!(server.hello_times(), [5, 1_005, 2_015]);
    assert_eq!(server.beat_times(), [3_025]);
}

/// A welcome with a zero heartbeat interval fails the wire's check: the agent ignores it,
/// counts it, and says hello again an interval later. Catches an agent that trusts a
/// welcome it did not validate.
#[test]
fn a_welcome_that_fails_validation_is_ignored_and_counted() {
    let script = Script::plain().hello(1, HelloAnswer::Invalid);
    let (sim, _) = run(spec(), script, 2_500);
    let server = stand_in(&sim);
    assert_eq!(server.hello_times(), [5, 1_005]);
    assert_eq!(server.beat_times(), [2_015]);
    assert_eq!(count(&sim, Counter::ReplyInvalid), 1);
}

/// The same welcome arrives a second time, 500 ms after the first: the agent already
/// holds the session and the second answers no open call, so it is ignored and the
/// heartbeats keep the first welcome's rhythm. Catches an agent that takes an answer to a
/// call it is no longer waiting on.
#[test]
fn an_answer_to_no_open_call_is_ignored() {
    let script = Script::plain().hello(1, HelloAnswer::Twice(SECOND_WELCOME_AFTER));
    let (sim, _) = run(spec(), script, 2_500);
    let server = stand_in(&sim);
    assert_eq!(server.hello_times(), [5]);
    assert_eq!(server.beat_times(), [1_015, 2_025]);
}

/// The stand-in answers the second heartbeat 409 `unknown-host`; the first was answered
/// 200, so the agent says hello again at once, is welcomed into a new session, and beats
/// in it. Catches an agent that does not follow the 409 rule (it keeps beating into a
/// session the server does not hold, or waits before it says hello).
#[test]
fn an_unknown_host_answer_to_a_heartbeat_is_followed_by_a_hello() {
    let refuse = BeatAnswer::Refuse {
        status: 409,
        code: "unknown-host",
    };
    let (sim, _) = run(spec(), Script::plain().beat(2, refuse), 4_500);
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
}

/// A 409 to the first heartbeat of a session, before any was accepted, is followed by a
/// hello one interval later, not at once. Catches an agent that would ask a server that
/// welcomes and at once refuses in a loop.
#[test]
fn a_refusal_straight_after_a_welcome_waits_an_interval_before_the_next_hello() {
    let refuse = BeatAnswer::Refuse {
        status: 409,
        code: "stale-epoch",
    };
    let (sim, _) = run(spec(), Script::plain().beat(1, refuse), 3_500);
    let server = stand_in(&sim);
    assert_eq!(server.hello_times(), [5, 2_025]);
    assert_eq!(server.beat_times(), [1_015, 3_035]);
}

/// A 500 and a 403 with another code leave the session as it is; a 403
/// `session-mismatch` ends it and the agent says hello again at once. Catches an agent
/// that ends a session on any refusal, or ignores the session-mismatch rule.
#[test]
fn a_session_mismatch_answer_is_followed_by_a_hello_and_other_refusals_are_not() {
    let script = Script::plain()
        .beat(
            2,
            BeatAnswer::Refuse {
                status: 500,
                code: "internal",
            },
        )
        .beat(
            3,
            BeatAnswer::Refuse {
                status: 403,
                code: "host-bound",
            },
        )
        .beat(
            4,
            BeatAnswer::Refuse {
                status: 403,
                code: "session-mismatch",
            },
        );
    let (sim, _) = run(spec(), script, 5_500);
    let server = stand_in(&sim);
    assert_eq!(server.hello_times(), [5, 4_055]);
    assert_eq!(server.beat_times(), [1_015, 2_025, 3_035, 4_045, 5_065]);
}

/// The first reply starts `sim-job-0`, which runs 1.5 s and ends at 2 520. Its result is in
/// the heartbeats of 3 030, 4 040 and 5 050 (the job listed as exited and unacknowledged),
/// the answer to the third acknowledges it `ACCEPTED`, and no later heartbeat carries it.
/// The ledger shows one launch, the heartbeats list no run and carry no run result, and the
/// agent runs nothing at the end. Catches a result dropped before it is acknowledged, or
/// resent after.
#[test]
fn a_start_is_launched_once_and_the_result_is_resent_until_acknowledged() {
    let script = Script::plain()
        .commands(1, json!([start(JOB0, SHORT_RUN_MS)]))
        .beat(5, acks(&[(JOB0, "ACCEPTED")]));
    let (sim, ledger) = run(job_spec(), script, 7_500);
    let server = stand_in(&sim);
    let none = jobs(&[]);
    let one = jobs(&[JOB0]);
    let carried = [&none, &none, &one, &one, &one, &none, &none].map(Clone::clone);
    assert_eq!(server.results(), carried);
    let beats = server.beats();
    assert_eq!(beats[1].1.jobs[0].state, HeldState::Running);
    assert_eq!(beats[2].1.jobs[0].state, HeldState::ExitedUnacked);
    assert!(beats[5].1.jobs.is_empty());
    let result = beats[2].1.jobs[0].result.as_ref().expect("a result");
    assert_eq!(result.allocation_id, ALLOCATION);
    assert_eq!(result.exit_code, Some(0));
    for (_, beat) in &beats {
        assert!(beat.held.is_empty());
        assert!(beat.results.is_empty());
    }
    assert_eq!(count(&sim, Counter::ResultResent), 2);
    assert_eq!(launches(&ledger), [(named(JOB0), 1_020)]);
    let truth = agent(&sim).truth().expect("the agent runs");
    assert!(truth.running.is_empty());
    assert!(truth.jobs.is_empty());
}

/// An acknowledgement of a job whose result does not exist yet changes nothing, one with a
/// verdict this version cannot read keeps the result, and the next one with a known verdict
/// ends it. Catches an agent that drops a result on a verdict it does not know.
#[test]
fn acknowledgements_that_do_not_end_a_result_leave_it_to_be_resent() {
    let script = Script::plain()
        .commands(1, json!([start(JOB0, SHORT_RUN_MS)]))
        .beat(2, acks(&[(JOB0, "ACCEPTED")]))
        .beat(3, acks(&[(JOB0, "SOMETHING_NEWER")]))
        .beat(4, acks(&[(JOB0, "STALE")]));
    let (sim, _) = run(job_spec(), script, 5_500);
    let none = jobs(&[]);
    let one = jobs(&[JOB0]);
    let carried = [&none, &none, &one, &one, &none].map(Clone::clone);
    assert_eq!(stand_in(&sim).results(), carried);
}

/// The same start arrives in two replies. A correct agent launches it once and counts
/// the repeat; one that runs a repeated start launches it twice, and the cell's checker
/// finds the second launch in the ledger. Catches a node that does not pass the repeat
/// to the agent, or a ledger that does not record each launch.
#[test]
fn a_repeated_start_is_not_launched_twice_by_a_correct_agent_and_is_by_a_defective_one() {
    let script = Script::plain()
        .commands(1, json!([start(JOB0, LONG_RUN_MS)]))
        .commands(2, json!([start(JOB0, LONG_RUN_MS)]));
    let (sim, ledger) = run(job_spec(), script.clone(), 3_500);
    assert_eq!(launches(&ledger), [(named(JOB0), 1_020)]);
    assert_eq!(count(&sim, Counter::StartRepeated), 1);
    assert_eq!(count(&sim, Counter::StartAfterStop), 0);
    assert!(ledger_violations(&ledger, 3_500).is_empty());

    let (sim, ledger) = run(broken(Defect::StartRunsTwice), script, 3_500);
    let twice = [(named(JOB0), 1_020), (named(JOB0), 2_030)];
    assert_eq!(launches(&ledger), twice);
    assert_eq!(count(&sim, Counter::StartRepeated), 1);
    assert_eq!(
        ledger_violations(&ledger, 3_500),
        [Invariant::AtMostOneLaunch]
    );
}

/// The worker takes 1.5 s to act on a reply. The reply to heartbeat 1 (read at 1 020) starts
/// `sim-job-0` at 2 520; heartbeat 2 left at 2 020 without it, so its reply starts the job
/// again, and the worker, busy until 2 520, acts on that one at 4 020: a repeat, counted and
/// not launched. Catches a node that acts on a reply the moment it reads it, one that lets
/// the next reply overtake the one before it, and one that counts the repeat at the wrong
/// time.
#[test]
fn a_worker_with_a_lag_acts_on_replies_in_order_after_the_lag() {
    let mut slow = job_spec();
    slow.worker_lag = Duration::from_millis(1_500);
    let script = Script::plain()
        .commands(1, json!([start(JOB0, LONG_RUN_MS)]))
        .commands(2, json!([start(JOB0, LONG_RUN_MS)]));
    let (mut sim, ledger) = cell(slow, script, INTERVAL);
    sim.run_until(ms(2_400));
    assert!(launches(&ledger).is_empty());
    sim.run_until(ms(3_900));
    assert_eq!(launches(&ledger), [(named(JOB0), 2_520)]);
    assert_eq!(count(&sim, Counter::StartRepeated), 0);
    sim.run_until(ms(4_100));
    assert_eq!(launches(&ledger), [(named(JOB0), 2_520)]);
    assert_eq!(count(&sim, Counter::StartRepeated), 1);
    let beats = stand_in(&sim).beats();
    assert!(
        beats[1].1.jobs.is_empty(),
        "heartbeat 2 left before the worker acted"
    );
}

/// A stop for `sim-job-0`, then a start for it. A correct agent retired the id and launches
/// nothing; one that forgets retired ids launches it. Both count the start after the stop.
/// Catches a node that does not hand the stop to the agent, or the start after it.
#[test]
fn a_start_for_a_stopped_job_is_not_launched_by_a_correct_agent_and_is_by_a_defective_one() {
    let script = Script::plain()
        .commands(1, json!([stop(JOB0)]))
        .commands(2, json!([start(JOB0, LONG_RUN_MS)]));
    let (sim, ledger) = run(job_spec(), script.clone(), 3_500);
    assert!(launches(&ledger).is_empty());
    assert_eq!(count(&sim, Counter::StartAfterStop), 1);
    assert_eq!(count(&sim, Counter::StartRepeated), 0);

    let (sim, ledger) = run(broken(Defect::ForgetsRetiredIds), script, 3_500);
    assert_eq!(launches(&ledger), [(named(JOB0), 2_030)]);
    assert_eq!(count(&sim, Counter::StartAfterStop), 1);
}

/// `sim-job-0` ends at 2 520 while the answer to heartbeat 2 (sent at 2 020) is cut. The
/// agent has one call in flight, so it sends nothing until the request limit (10 s) has
/// passed, at 12 020, and is then told its connection dropped; heartbeat 3 leaves a backoff
/// (1 s) later. A correct agent still carries the result in heartbeat 3, and one that
/// forgets its unacknowledged results on a drop does not. Catches a node that never calls
/// `disconnect`, one that sends before the limit, and one that sends after the limit
/// with no wait.
#[test]
fn a_heartbeat_with_no_reply_disconnects_the_agent_and_a_defective_one_forgets_its_result() {
    let script = Script::plain()
        .commands(1, json!([start(JOB0, SHORT_RUN_MS)]))
        .beat(2, BeatAnswer::Silent);
    let (sim, _) = run(job_spec(), script.clone(), 13_500);
    let none = jobs(&[]);
    let one = jobs(&[JOB0]);
    let carried = [&none, &none, &one].map(Clone::clone);
    assert_eq!(stand_in(&sim).results(), carried);
    assert_eq!(stand_in(&sim).beat_times(), [1_015, 2_025, 13_025]);

    let (sim, _) = run(broken(Defect::ForgetsUnackedResult), script, 13_500);
    let carried = [&none, &none, &none].map(Clone::clone);
    assert_eq!(stand_in(&sim).results(), carried);
}

/// A reply whose only command is a start of an image not pinned by digest fails the
/// wire's check: nothing is launched and the reply is counted. Catches a node that acts
/// on a command before validating it.
#[test]
fn a_reply_that_fails_validation_is_ignored_and_counted() {
    let mut unpinned = start(JOB0, LONG_RUN_MS);
    unpinned["image"] = json!("busybox:1");
    let script = Script::plain().commands(1, json!([unpinned]));
    let (sim, ledger) = run(job_spec(), script, 2_500);
    assert!(launches(&ledger).is_empty());
    assert!(agent(&sim).truth().expect("runs").jobs.is_empty());
    assert_eq!(count(&sim, Counter::ReplyInvalid), 1);
}

/// Answers the agent must ignore in whole or in part: a body that is not JSON; a reply
/// whose acknowledgements name one job twice (the acknowledgements are ignored, the start
/// beside them still runs); a command this version does not know, and the `start`, `stop`
/// and `cancel` an older server sends (skipped, not counted: they pass the wire's check as
/// commands of no meaning); and a start in an allocation the cell's server would not write.
/// Catches a node that trusts a bad answer, or drops a good command beside a bad part.
#[test]
fn answers_that_fail_to_decode_or_carry_bad_parts_are_counted() {
    let mut odd_allocation = start(JOB2, LONG_RUN_MS);
    odd_allocation["allocation_id"] = json!("allocation");
    let double_ack = json!({
        "commands": [start(JOB1, LONG_RUN_MS)],
        "job_acks": [
            { "job_id": "sim-job-9", "ack": "ACCEPTED" },
            { "job_id": "sim-job-9", "ack": "ACCEPTED" },
        ],
    });
    let older = json!([
        { "type": "reboot", "job_id": JOB0 },
        { "type": "start", "run_id": "r1" },
        { "type": "stop", "run_id": "r1" },
        { "type": "cancel", "run_id": "r1" },
    ]);
    let script = Script::plain()
        .beat(1, BeatAnswer::Raw("not json"))
        .beat(2, BeatAnswer::Reply(double_ack))
        .commands(3, older)
        .commands(4, json!([odd_allocation]));
    let (sim, ledger) = run(job_spec(), script, 5_500);
    assert_eq!(launches(&ledger), [(named(JOB1), 2_030)]);
    assert_eq!(count(&sim, Counter::ReplyInvalid), 2);
}

/// The answer to heartbeat 2 (sent at 2 020) is held back 2.5 s. The agent has one call in
/// flight: heartbeat 3 leaves an interval after the late answer is read, at 5 530, not
/// at 3 020, and the start in the answer is acted on when it is read, at 4 530. Catches an
/// agent that sends a heartbeat while the last has no answer, or drops a late one that
/// is inside the request limit.
#[test]
fn an_agent_sends_no_heartbeat_while_the_last_has_no_answer() {
    let late = json!({ "commands": [start(JOB0, LONG_RUN_MS)] });
    let script = Script::plain().beat(2, BeatAnswer::Late(HELD_BACK, late));
    let (sim, ledger) = run(job_spec(), script, 6_000);
    let server = stand_in(&sim);
    assert_eq!(server.beat_times(), [1_015, 2_025, 5_535]);
    assert_eq!(launches(&ledger), [(named(JOB0), 4_530)]);
}

/// The answer to heartbeat 2 is held back 11 s, past the request limit (10 s): the agent
/// gave the call up at 12 020, and the answer, which arrives at 13 030, is dropped unread,
/// so its start launches nothing. Heartbeat 3 leaves at 13 020, a second after the limit,
/// and heartbeat 4 an interval after its answer. Catches an agent that acts on an answer to a
/// call it gave up.
#[test]
fn an_answer_that_comes_after_the_request_limit_is_dropped() {
    let late = json!({ "commands": [start(JOB0, LONG_RUN_MS)] });
    let script = Script::plain().beat(2, BeatAnswer::Late(Duration::from_secs(11), late));
    let (sim, ledger) = run(job_spec(), script, 14_500);
    let server = stand_in(&sim);
    assert_eq!(server.beat_times(), [1_015, 2_025, 13_025, 14_035]);
    assert!(launches(&ledger).is_empty());
}

/// Two jobs launch at 1 020. The process is killed at 2 012 (the answer to heartbeat 2
/// arrives at 2 020 and is dropped, as is the first job's end at 3 020), and replaced at
/// 4 500. The replacement holds nothing, says hello again at the node's next tick (5 020)
/// with nothing held, is welcomed into a new session and beats with nothing to report, and
/// the second job's end timer, armed by the dead process, does nothing. The cell's ledger
/// still shows both launches. Catches a ledger kept in the agent process, and a replacement
/// that inherits its predecessor's state or timers.
#[test]
fn a_killed_agent_loses_its_state_and_a_replacement_registers_empty_and_the_ledger_remembers() {
    let script = Script::plain().commands(1, json!([start(JOB0, 2_000), start(JOB1, 9_000)]));
    let (mut sim, ledger) = cell(job_spec(), script, INTERVAL);
    sim.run_until(ms(2_012));
    assert_eq!(agent(&sim).truth().expect("alive").jobs.len(), 2);
    agent_mut(&mut sim).kill();
    assert!(agent(&sim).truth().is_none());
    sim.run_until(ms(4_500));
    assert!(agent(&sim).truth().is_none());
    agent_mut(&mut sim).replace();
    let fresh = agent(&sim).truth().expect("replaced");
    assert!(fresh.jobs.is_empty());
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

/// A call addressed to the agent (only the server takes calls) reaches a live process: the
/// node ignores it, sending nothing and counting nothing. Catches a node that takes a call
/// for an answer.
#[test]
fn a_call_addressed_to_the_agent_is_ignored() {
    let mut node = AgentNode::new(spec(), Ledger::new());
    let call = Msg::Request {
        id: CallId(0),
        route: Route::Hello,
        bearer: JOIN.to_owned(),
        body: String::new(),
    };
    let from = NodeId::new(SERVER);
    let event = Event::Message { from, msg: call };
    let input = NodeInput {
        now: ms(10),
        entropy: 0,
        event,
    };
    assert!(node.apply(input).is_empty());
    assert!(node.take_outputs().is_empty());
    assert_eq!(node.counters().get(Counter::ReplyInvalid), 0);
}

/// The answer to heartbeat 1 starts `sim-job-0`, which the next hello or heartbeat has not
/// yet listed: from the start until heartbeat 2 leaves (at 2 010) the agent runs a job the
/// server cannot have seen listed, and after it, which names the job held, it does not.
/// Catches an agent that counts a job listed before it was, or one never listed.
#[test]
fn a_started_job_is_unlisted_until_a_heartbeat_lists_it() {
    let script = Script::plain().commands(1, json!([start(JOB0, LONG_RUN_MS)]));
    let (mut sim, _) = cell(job_spec(), script, INTERVAL);
    sim.run_until(ms(500));
    assert!(!agent(&sim).runs_unlisted());
    sim.run_until(ms(1_500));
    assert!(agent(&sim).runs_unlisted());
    sim.run_until(ms(2_500));
    assert!(!agent(&sim).runs_unlisted());
}

/// The same script twice gives the same trace hash; the trace names every call and
/// answer by route, number and status, and no line holds the join token or the session
/// secret. Catches a trace that leaks a secret, or a node whose run depends on anything
/// but the seed and the script.
#[test]
fn the_trace_of_an_agent_run_replays_and_holds_no_secret() {
    let script = Script::plain()
        .commands(1, json!([start(JOB0, SHORT_RUN_MS)]))
        .beat(3, acks(&[(JOB0, "ACCEPTED")]));
    let (first, _) = run(job_spec(), script.clone(), 5_500);
    let (second, _) = run(job_spec(), script, 5_500);
    assert_eq!(first.trace_hash(), second.trace_hash());
    let lines = first.trace_lines().expect("the trace is kept");
    assert_eq!(lines, second.trace_lines().expect("the trace is kept"));
    let name = agent_name(0);
    let hello = format!("deliver #0 {name}->server call #0 /v1/agent/hello");
    assert!(
        lines.iter().any(|line| line.ends_with(&hello)),
        "{lines:#?}"
    );
    let welcome = format!("deliver #1 server->{name} answer #0 200");
    assert!(
        lines.iter().any(|line| line.ends_with(&welcome)),
        "{lines:#?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("/v1/agent/heartbeat"))
    );
    for line in lines {
        assert!(!line.contains(JOIN), "{line}");
        assert!(!line.contains(SESSION_PREFIX), "{line}");
        assert!(!line.contains("credential"), "{line}");
    }
}
