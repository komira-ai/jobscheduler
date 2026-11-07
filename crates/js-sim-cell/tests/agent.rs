//! The agent node against a scripted stand-in for the server: each behaviour is shown by
//! an answer the test chose, at an exact virtual time.
//!
//! Every message takes the stand-in's `DELAY` (5 ms) and the welcome names [`INTERVAL`]
//! (1 s), so the times below follow from the script: the hello leaves at 0 and is
//! welcomed at 10, and heartbeat `n` of that session leaves at `10 + n * 1000`, reaches
//! the stand-in 5 ms later, and its answer is back 5 ms after that.

use std::collections::BTreeMap;
use std::time::Duration;

use js_agent_wire::{HeldState, RETRY_AFTER_CAP};
use js_conformance::agent::Defect;
use js_sim::Sim;
use js_sim_cell::bus::agent_name;
use js_sim_cell::{
    AgentKind, AgentSpec, Checker, Counter, Invariant, Ledger, Observations, RunId, SIM_RUN_IMAGE,
    Touched, Verdict,
};
use js_types::sched::FarmTime;
use js_types::{HostId, LeaseId, Resources};
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

/// How long a run that should outlast the test runs.
const LONG_RUN_MS: u64 = 60_000;

/// How long a run that ends inside the test runs.
const SHORT_RUN_MS: u64 = 1_500;

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
    }
}

/// The agent of [`spec`], lacking `defect`.
fn broken(defect: Defect) -> AgentSpec {
    AgentSpec {
        kind: AgentKind::Fake(Some(defect)),
        ..spec()
    }
}

/// A start of `run` in lease `1-1` that runs `millis` by itself, naming heartbeat `seq`
/// and valid for `valid_for` ms (0: unbounded).
fn start(run: &str, millis: u64, seq: u64, valid_for: u64) -> Value {
    json!({
        "type": "start",
        "run_id": run,
        "lease_id": "1-1",
        "image": SIM_RUN_IMAGE,
        "args": [millis.to_string()],
        "limits": { "cpu_millis": 1000, "memory_bytes": MIB, "wall_ms": 600_000 },
        "heartbeat_seq": seq,
        "valid_for_ms": valid_for,
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

/// Each completion in the ledger as its run and time.
fn completions(ledger: &Ledger) -> Vec<(String, u64)> {
    let each = |done: &js_sim_cell::Completion| {
        assert_eq!(done.host.as_str(), "h000");
        (done.run.as_str().to_owned(), done.at.as_millis())
    };
    ledger.completions().iter().map(each).collect()
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

fn runs(names: &[&str]) -> Vec<String> {
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
    assert_eq!(server.beat_times(), [3_015, 4_015, 5_015, 6_015]);
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
    assert_eq!(server.beat_times(), [third + 1_015, third + 2_015]);
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
    assert_eq!(server.beat_times(), [1_015, 2_015]);
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
    assert_eq!(server.hello_times(), [5, 2_025]);
    assert_eq!(server.beat_times(), [1_015, 2_015, 3_035, 4_035]);
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
    assert_eq!(server.hello_times(), [5, 4_025]);
    assert_eq!(server.beat_times(), [1_015, 2_015, 3_015, 4_015, 5_035]);
}

/// The first reply starts `r1`, which runs 1.5 s and ends at 2 520. Its result is in the
/// heartbeats of 3 010, 4 010 and 5 010 (the run held as exited and unacknowledged), the
/// answer to the third acknowledges it `ACCEPTED`, and no later heartbeat carries it. The
/// ledger shows one launch and one completion, and the ground truth the verdict. Catches a
/// result dropped before it is acknowledged, or resent after.
#[test]
fn a_start_is_launched_once_and_the_result_is_resent_until_acknowledged() {
    let script = Script::plain()
        .commands(1, json!([start("r1", SHORT_RUN_MS, 1, 0)]))
        .beat(5, acks(&[("r1", "ACCEPTED")]));
    let (sim, ledger) = run(spec(), script, 7_500);
    let server = stand_in(&sim);
    let none = runs(&[]);
    let r1 = runs(&["r1"]);
    let carried = [&none, &none, &r1, &r1, &r1, &none, &none].map(Clone::clone);
    assert_eq!(server.results(), carried);
    let beats = server.beats();
    assert_eq!(beats[1].1.held[0].state, HeldState::Running);
    assert_eq!(beats[2].1.held[0].state, HeldState::ExitedUnacked);
    assert!(beats[5].1.held.is_empty());
    let result = &beats[2].1.results[0];
    assert_eq!(result.lease_id, "1-1");
    assert_eq!(result.exit_code, Some(0));
    assert_eq!(count(&sim, Counter::ResultResent), 2);
    assert_eq!(launches(&ledger), [("r1".to_owned(), 1_020)]);
    assert_eq!(completions(&ledger), [("r1".to_owned(), 2_520)]);
    let truth = agent(&sim).truth().expect("the agent runs");
    assert!(truth.running.is_empty());
    assert_eq!(
        truth.completed,
        BTreeMap::from([(RunId::new("r1"), ms(2_520))])
    );
    let accepted = BTreeMap::from([(RunId::new("r1"), Verdict::Accepted)]);
    assert_eq!(truth.acknowledged, accepted);
}

/// An acknowledgement of a run whose result does not exist yet changes nothing, one with
/// a verdict this version cannot read keeps the result, and the next one with a known
/// verdict ends it and is the verdict the ground truth records. Catches an agent that
/// drops a result on a verdict it does not know.
#[test]
fn acknowledgements_that_do_not_end_a_result_leave_it_to_be_resent() {
    let script = Script::plain()
        .commands(1, json!([start("r1", SHORT_RUN_MS, 1, 0)]))
        .beat(2, acks(&[("r1", "ACCEPTED")]))
        .beat(3, acks(&[("r1", "SOMETHING_NEWER")]))
        .beat(4, acks(&[("r1", "STALE")]));
    let (sim, _) = run(spec(), script, 5_500);
    let none = runs(&[]);
    let r1 = runs(&["r1"]);
    let carried = [&none, &none, &r1, &r1, &none].map(Clone::clone);
    assert_eq!(stand_in(&sim).results(), carried);
    let truth = agent(&sim).truth().expect("the agent runs");
    let stale = BTreeMap::from([(RunId::new("r1"), Verdict::Stale)]);
    assert_eq!(truth.acknowledged, stale);
}

/// The same start arrives in two replies. A correct agent launches it once and counts
/// the repeat; one that runs a repeated start launches it twice, and the cell's checker
/// finds the second launch in the ledger. Catches a node that does not pass the repeat
/// to the agent, or a ledger that does not record each launch.
#[test]
fn a_repeated_start_is_not_launched_twice_by_a_correct_agent_and_is_by_a_defective_one() {
    let script = Script::plain()
        .commands(1, json!([start("r1", LONG_RUN_MS, 1, 0)]))
        .commands(2, json!([start("r1", LONG_RUN_MS, 1, 0)]));
    let (sim, ledger) = run(spec(), script.clone(), 3_500);
    assert_eq!(launches(&ledger), [("r1".to_owned(), 1_020)]);
    assert_eq!(count(&sim, Counter::StartRepeated), 1);
    assert_eq!(count(&sim, Counter::StartAfterStop), 0);
    assert!(ledger_violations(&ledger, 3_500).is_empty());

    let (sim, ledger) = run(broken(Defect::StartRunsTwice), script, 3_500);
    let twice = [("r1".to_owned(), 1_020), ("r1".to_owned(), 2_020)];
    assert_eq!(launches(&ledger), twice);
    assert_eq!(count(&sim, Counter::StartRepeated), 1);
    assert_eq!(
        ledger_violations(&ledger, 3_500),
        [Invariant::AtMostOneLaunch]
    );
}

/// A stop for `r1`, then a start for it. A correct agent retired the id and launches
/// nothing; one that forgets retired ids launches it. Both count the start after the stop.
/// Catches a node that does not hand the stop to the agent, or the start after it.
#[test]
fn a_start_for_a_stopped_run_is_not_launched_by_a_correct_agent_and_is_by_a_defective_one() {
    let script = Script::plain()
        .commands(1, json!([{ "type": "stop", "run_id": "r1" }]))
        .commands(2, json!([start("r1", LONG_RUN_MS, 2, 0)]));
    let (sim, ledger) = run(spec(), script.clone(), 3_500);
    assert!(launches(&ledger).is_empty());
    assert_eq!(count(&sim, Counter::StartAfterStop), 1);
    assert_eq!(count(&sim, Counter::StartRepeated), 0);

    let (sim, ledger) = run(broken(Defect::ForgetsRetiredIds), script, 3_500);
    assert_eq!(launches(&ledger), [("r1".to_owned(), 2_020)]);
    assert_eq!(count(&sim, Counter::StartAfterStop), 1);
}

/// The answer to heartbeat 3 (sent at 3 010) carries two starts valid for 1.5 s: `late`
/// names heartbeat 1 (sent at 1 010, so it waited 2 010 ms) and `fresh` names heartbeat 3
/// (it waited 10 ms). A correct agent drops `late` and counts it; one that runs late
/// starts launches both. Catches `waited` counted from the heartbeat the reply answers
/// (then `late` runs) or from the hello (then `fresh` is dropped too).
#[test]
fn a_start_that_waited_past_its_validity_is_dropped() {
    let script = Script::plain().commands(
        3,
        json!([
            start("late", LONG_RUN_MS, 1, 1_500),
            start("fresh", LONG_RUN_MS, 3, 1_500),
        ]),
    );
    let (sim, ledger) = run(spec(), script.clone(), 3_500);
    assert_eq!(launches(&ledger), [("fresh".to_owned(), 3_020)]);
    assert_eq!(count(&sim, Counter::StartDroppedLate), 1);

    let (sim, ledger) = run(broken(Defect::RunsLateStart), script, 3_500);
    let both = [("late".to_owned(), 3_020), ("fresh".to_owned(), 3_020)];
    assert_eq!(launches(&ledger), both);
    assert_eq!(count(&sim, Counter::StartDroppedLate), 0);
}

/// A start that names a heartbeat the agent never sent counts its wait from the hello
/// that opened the session: at 3 020 it waited 3 020 ms, past its 1.5 s. Catches a wait
/// read as zero for a number the agent cannot place, which would run a stale start.
#[test]
fn a_start_naming_a_heartbeat_never_sent_counts_from_the_hello() {
    let script = Script::plain().commands(3, json!([start("r1", LONG_RUN_MS, 99, 1_500)]));
    let (sim, ledger) = run(spec(), script, 3_500);
    assert!(launches(&ledger).is_empty());
    assert_eq!(count(&sim, Counter::StartDroppedLate), 1);
}

/// `r1` ends at 2 520 while the answers to heartbeats 2 and 3 are cut. When heartbeat 3
/// is due, heartbeat 2 has had no answer, so the agent is told its connection dropped:
/// a correct agent still carries the result in heartbeat 3, and one that forgets its
/// unacknowledged results on a drop does not. Catches a node that never calls
/// `disconnect`.
#[test]
fn a_heartbeat_with_no_reply_disconnects_the_agent_and_a_defective_one_forgets_its_result() {
    let script = Script::plain()
        .commands(1, json!([start("r1", SHORT_RUN_MS, 1, 0)]))
        .beat(2, BeatAnswer::Silent)
        .beat(3, BeatAnswer::Silent);
    let (sim, _) = run(spec(), script.clone(), 4_500);
    let none = runs(&[]);
    let r1 = runs(&["r1"]);
    let carried = [&none, &none, &r1, &r1].map(Clone::clone);
    assert_eq!(stand_in(&sim).results(), carried);

    let (sim, _) = run(broken(Defect::ForgetsUnackedResult), script, 4_500);
    let carried = [&none, &none, &none, &none].map(Clone::clone);
    assert_eq!(stand_in(&sim).results(), carried);
}

/// A reply whose only command is a start of an image not pinned by digest fails the
/// wire's check: nothing is launched and the reply is counted. Catches a node that acts
/// on a command before validating it.
#[test]
fn a_reply_that_fails_validation_is_ignored_and_counted() {
    let mut unpinned = start("r1", LONG_RUN_MS, 1, 0);
    unpinned["image"] = json!("busybox:1");
    let script = Script::plain().commands(1, json!([unpinned]));
    let (sim, ledger) = run(spec(), script, 2_500);
    assert!(launches(&ledger).is_empty());
    assert!(agent(&sim).truth().expect("runs").running.is_empty());
    assert_eq!(count(&sim, Counter::ReplyInvalid), 1);
    assert_eq!(count(&sim, Counter::StartDroppedLate), 0);
}

/// Answers the agent must ignore in whole or in part: a body that is not JSON; a reply
/// whose acknowledgements name one run twice (the acknowledgements are ignored, the start
/// beside them still runs); a command this version does not know (skipped, not counted:
/// it passes the wire's check); a start in a lease the cell's server would not write; and
/// a start asking for a GPU on a host that has none. Catches a node that trusts a bad
/// answer, or drops a good command beside a bad part.
#[test]
fn answers_that_fail_to_decode_or_carry_bad_parts_are_counted() {
    let mut no_gpu = spec();
    no_gpu.capacity = Resources::new(8_000, 16 * GIB, 0, None).expect("capacity");
    let mut odd_lease = start("r4", LONG_RUN_MS, 4, 0);
    odd_lease["lease_id"] = json!("lease");
    let mut gpu = start("r5", LONG_RUN_MS, 5, 0);
    gpu["gpu_indices"] = json!([0]);
    let double_ack = json!({
        "commands": [start("r2", LONG_RUN_MS, 2, 0)],
        "result_acks": [
            { "run_id": "r9", "ack": "ACCEPTED" },
            { "run_id": "r9", "ack": "ACCEPTED" },
        ],
    });
    let script = Script::plain()
        .beat(1, BeatAnswer::Raw("not json"))
        .beat(2, BeatAnswer::Reply(double_ack))
        .commands(3, json!([{ "type": "reboot", "run_id": "r3" }]))
        .commands(4, json!([odd_lease]))
        .commands(5, json!([gpu]));
    let (sim, ledger) = run(no_gpu, script, 5_500);
    assert_eq!(launches(&ledger), [("r2".to_owned(), 2_020)]);
    assert_eq!(count(&sim, Counter::ReplyInvalid), 4);
}

/// The answer to heartbeat 2 is held back 2.5 s. Heartbeats 3 and 4 are sent on time
/// regardless, and the start in the late answer is acted on when it arrives, at 4 520.
/// Catches an agent that waits for an answer before its next heartbeat, or drops a late
/// one.
#[test]
fn an_agent_beats_on_the_interval_even_when_the_previous_reply_is_late() {
    let late = json!({ "commands": [start("r1", LONG_RUN_MS, 2, 0)] });
    let script = Script::plain().beat(2, BeatAnswer::Late(HELD_BACK, late));
    let (sim, ledger) = run(spec(), script, 5_500);
    let server = stand_in(&sim);
    assert_eq!(server.beat_times(), [1_015, 2_015, 3_015, 4_015, 5_015]);
    assert_eq!(launches(&ledger), [("r1".to_owned(), 4_520)]);
}

/// `a` runs 3 s and `b` runs 1 s, each with its start's resources, in its lease. A
/// cancel of `a` before its end and a stop of `b` end both with no result: no heartbeat
/// carries one, `a`'s end passes unnoticed and the ledger has no completion. Catches a
/// node that keeps a stopped run in its ground truth, completes it at its time, or
/// misreads the resources a start holds.
#[test]
fn a_stopped_or_cancelled_run_ends_without_a_result() {
    let mut a = start("a", 3_000, 1, 0);
    a["lease_id"] = json!("2-5");
    let mut b = start("b", 0, 1, 0);
    b["image"] =
        json!("busybox@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef");
    b["args"] = json!(["sleep"]);
    b["gpu_indices"] = json!([0]);
    b["limits"] = json!({
        "cpu_millis": 2000,
        "memory_bytes": GIB,
        "disk_bytes": MIB,
        "wall_ms": 1000,
    });
    let script = Script::plain().commands(1, json!([a, b])).commands(
        2,
        json!([
            { "type": "cancel", "run_id": "a" },
            { "type": "stop", "run_id": "b" },
        ]),
    );
    let (mut sim, ledger) = cell(spec(), script, INTERVAL);
    sim.run_until(ms(1_500));
    let truth = agent(&sim).truth().expect("the agent runs");
    let a_holds = Resources::new(1_000, MIB, 0, None).expect("a");
    let b_holds = Resources::new(2_000, GIB, 1, Some("nvidia-a100".to_owned()))
        .expect("b")
        .with_disk_bytes(MIB);
    let held: Vec<(&str, LeaseId, Resources)> = truth
        .running
        .iter()
        .map(|(run, running)| (run.as_str(), running.lease, running.resources.clone()))
        .collect();
    let wanted = [
        ("a", LeaseId::new(2, 5), a_holds),
        ("b", LeaseId::new(1, 1), b_holds),
    ];
    assert_eq!(held, wanted);
    assert_eq!(truth.capacity, spec().capacity);
    sim.run_until(ms(6_500));
    let truth = agent(&sim).truth().expect("the agent runs");
    assert!(truth.running.is_empty());
    assert!(truth.completed.is_empty());
    assert!(completions(&ledger).is_empty());
    assert!(stand_in(&sim).results().iter().all(Vec::is_empty));
    let last = stand_in(&sim).beats().pop().expect("a heartbeat").1;
    assert!(last.held.is_empty());
}

/// Two runs launch at 1 020. The process is killed at 2 012 (the answer to heartbeat 2
/// arrives at 2 020 and is dropped, as is `a`'s end at 3 020), and replaced at 4 500. The
/// replacement holds nothing, says hello again at the node's next tick (5 010) with
/// nothing held, is welcomed into a new session and beats with nothing to report, and
/// `b`'s end timer, armed by the dead process, does nothing. The cell's ledger still
/// shows both launches. Catches a ledger kept in the agent process, and a replacement
/// that inherits its predecessor's state or timers.
#[test]
fn a_killed_agent_loses_its_state_and_a_replacement_registers_empty_and_the_ledger_remembers() {
    let script =
        Script::plain().commands(1, json!([start("a", 2_000, 1, 0), start("b", 9_000, 1, 0)]));
    let (mut sim, ledger) = cell(spec(), script, INTERVAL);
    sim.run_until(ms(2_012));
    assert_eq!(agent(&sim).truth().expect("alive").running.len(), 2);
    agent_mut(&mut sim).kill();
    assert!(agent(&sim).truth().is_none());
    sim.run_until(ms(4_500));
    assert!(agent(&sim).truth().is_none());
    agent_mut(&mut sim).replace();
    let fresh = agent(&sim).truth().expect("replaced");
    assert!(fresh.running.is_empty());
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

/// The same script twice gives the same trace hash; the trace names every call and
/// answer by route, number and status, and no line holds the join token or the session
/// secret. Catches a trace that leaks a secret, or a node whose run depends on anything
/// but the seed and the script.
#[test]
fn the_trace_of_an_agent_run_replays_and_holds_no_secret() {
    let script = Script::plain()
        .commands(1, json!([start("r1", SHORT_RUN_MS, 1, 0)]))
        .beat(3, acks(&[("r1", "ACCEPTED")]));
    let (first, _) = run(spec(), script.clone(), 5_500);
    let (second, _) = run(spec(), script, 5_500);
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
