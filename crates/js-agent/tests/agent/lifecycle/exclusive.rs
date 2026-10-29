//! The agent's side of a whole-node lease: the gate on `Start`, the quiet check of
//! `Preparing` and the clean-up check of `Restoring`. Driven in memory like the rest of the
//! lifecycle: a fake runtime, a clock that moves when a test moves it, and the scripted
//! post-job checks, which note in the log each time they run.

use js_agent::lifecycle::{CheckFinding, Event, Quiet, Restored, code};
use js_agent_wire::{Command, JobSpec, Outcome, refusal};
use serde_json::json;

use super::{Harness, Options, job, names, reply, reply_json};

/// The labels of a container no job can stand for (its run id names a path) that is not a
/// host job's: it is removed, and when it cannot be it stays, booked.
const ODD: [(&str, &str); 2] = [
    ("jobscheduler.dev/run-id", "../x"),
    ("jobscheduler.dev/isolation", "contained"),
];

const LEASE: &str = "whole-1";

/// A start for `run_id` that belongs to the lease `lease`.
fn of(lease: &str, run_id: &str) -> JobSpec {
    JobSpec {
        lease_id: lease.to_owned(),
        ..job(run_id)
    }
}

/// A reply that names the lease and carries `commands`.
fn granting(lease: &str, commands: Vec<Command>) -> js_agent_wire::HeartbeatReply {
    js_agent_wire::HeartbeatReply {
        exclusive: Some(lease.to_owned()),
        ..reply(commands)
    }
}

fn start(spec: JobSpec) -> Command {
    Command::Start(Box::new(spec))
}

/// A reply that keeps naming the lease and acknowledges the result of `run_id`, as the
/// server's replies do for as long as it holds the host.
fn ack_under(h: &mut Harness, run_id: &str) {
    let ack = reply_json(&json!({
        "exclusive": LEASE,
        "result_acks": [{ "run_id": run_id, "ack": "ACCEPTED" }]
    }));
    h.life.apply(&ack);
}

fn prepare(h: &mut Harness) -> (Quiet, Vec<Event>) {
    let mut events = Vec::new();
    (h.life.prepare(LEASE, &mut events), events)
}

fn restore(h: &mut Harness) -> (Restored, Vec<Event>) {
    let mut events = Vec::new();
    (h.life.restore(&mut events), events)
}

/// How many times the scripted checks ran for `run_id`.
fn checks_of(h: &Harness, run_id: &str) -> usize {
    let prefix = format!("check {run_id} ");
    h.log
        .borrow()
        .iter()
        .filter(|line| line.starts_with(&prefix))
        .count()
}

fn finding_for(run_id: &'static str) -> Options {
    Options {
        findings: vec![(run_id, CheckFinding::residue("stray-process", "pid 9"))],
        ..Options::default()
    }
}

/// Asserts that from the reply that names a lease the agent starts no run of any other
/// lease: the start is answered `failed_to_start` with the code `no-capacity`, nothing is
/// created, and the same start in a reply that names no lease runs. It catches a gate that
/// is missing (a run beside the whole-node lease), one that looks only at the first reply,
/// and one that is still closed after the lease ended.
#[test]
fn a_start_for_another_lease_is_refused_while_a_lease_holds_the_host() {
    let mut h = Harness::new("ex-other");
    let events = h
        .life
        .apply(&granting(LEASE, vec![start(of("other", "a"))]));
    assert_eq!(
        names(&events),
        [format!("refused a {}", refusal::NO_CAPACITY)]
    );
    assert!(h.fake.created().is_empty());
    let result = &h.life.results()[0];
    assert_eq!(result.outcome, Outcome::FailedToStart);
    assert_eq!(result.detail.as_deref(), Some(refusal::NO_CAPACITY));
    // Still named by a later reply: still refused.
    let events = h
        .life
        .apply(&granting(LEASE, vec![start(of("other", "b"))]));
    assert_eq!(
        names(&events),
        [format!("refused b {}", refusal::NO_CAPACITY)]
    );
    // The lease is gone: a start runs again.
    let events = h.life.apply(&reply(vec![start(of("other", "c"))]));
    assert_eq!(names(&events), ["started c"]);
}

/// Asserts that a start for the exclusive lease itself is refused until the host was found
/// quiet, and runs after: the consumer holds no token before readiness, so a start that
/// early is not the scheduler's. It catches a gate that lets the lease's own start through
/// before the checks ran, and one that keeps the lease out after they passed.
#[test]
fn a_start_for_the_lease_itself_runs_only_after_the_host_was_found_quiet() {
    let mut h = Harness::new("ex-own");
    let early = h.life.apply(&granting(LEASE, vec![start(of(LEASE, "a"))]));
    assert_eq!(
        names(&early),
        [format!("refused a {}", refusal::NO_CAPACITY)]
    );
    assert!(h.fake.created().is_empty());
    // The refusal is a result the server has not acknowledged: a run the agent holds.
    assert_eq!(prepare(&mut h).0, Quiet::Busy);
    ack_under(&mut h, "a");
    assert_eq!(prepare(&mut h).0, Quiet::Ready);
    let late = h.life.apply(&granting(LEASE, vec![start(of(LEASE, "b"))]));
    assert_eq!(names(&late), ["started b"]);
    // And no other lease, ready or not.
    let other = h
        .life
        .apply(&granting(LEASE, vec![start(of("other", "c"))]));
    assert_eq!(
        names(&other),
        [format!("refused c {}", refusal::NO_CAPACITY)]
    );
}

/// Asserts that a reply that names a lease changes nothing for a start in the same reply
/// that came before the lease in the list of commands: the lease is followed before the
/// commands, not after. It catches a gate that is opened one reply late.
#[test]
fn the_reply_that_grants_the_lease_gates_its_own_commands() {
    let mut h = Harness::new("ex-same-reply");
    let commands = vec![start(of("other", "a")), start(of("other", "b"))];
    let events = h.life.apply(&granting(LEASE, commands));
    assert_eq!(
        names(&events),
        [
            format!("refused a {}", refusal::NO_CAPACITY),
            format!("refused b {}", refusal::NO_CAPACITY)
        ]
    );
}

/// Asserts that a lease id the wire refuses neither starts a hold nor ends one: the agent
/// that was told nothing it can read goes on as it was. It catches a reply with a bad
/// `exclusive` that reads as "no lease" and so opens the host.
#[test]
fn an_unreadable_lease_id_changes_nothing() {
    let mut h = Harness::new("ex-bad");
    h.life
        .apply(&reply_json(&json!({ "exclusive": "not a lease" })));
    assert_eq!(h.life.exclusive(), None);
    h.life.apply(&granting(LEASE, vec![]));
    h.life
        .apply(&reply_json(&json!({ "exclusive": "not a lease" })));
    assert_eq!(h.life.exclusive(), Some(LEASE));
}

/// Asserts that a run the agent holds keeps the host busy through its whole life: running,
/// exited with its result unacknowledged, and (once acknowledged) gone; and that the
/// checks run once more, with the lease as the run, only when the last of it is gone. It
/// catches a quiet check that stops at "no container" while a result is owed, and one that
/// runs the checks while a job is still held.
#[test]
fn the_host_is_not_quiet_until_the_last_run_is_acknowledged() {
    let mut h = Harness::new("ex-busy");
    h.start(job("a"));
    h.life.apply(&granting(LEASE, vec![]));
    assert_eq!(prepare(&mut h).0, Quiet::Busy, "a run is running");
    h.fake.exit("a", 0);
    h.life.step();
    assert_eq!(h.life.held().len(), 1);
    assert_eq!(prepare(&mut h).0, Quiet::Busy, "its result is owed");
    assert_eq!(checks_of(&h, LEASE), 0);
    ack_under(&mut h, "a");
    assert!(h.held().is_empty());
    assert_eq!(prepare(&mut h).0, Quiet::Ready);
    assert_eq!(checks_of(&h, LEASE), 1);
}

/// Asserts that a container the agent could not remove makes the host unsafe, not busy: it
/// tainted the host, and waiting would not remove it. It catches a quiet check that hands
/// over a host with a container on it.
#[test]
fn a_container_that_cannot_be_removed_makes_the_host_unsafe() {
    let mut h = Harness::new("ex-stuck");
    h.fake.leak(&ODD, 0);
    h.fake.fail("remove", 100);
    h.restart().unwrap();
    h.life.apply(&granting(LEASE, vec![]));
    let (verdict, _) = prepare(&mut h);
    assert_eq!(
        verdict,
        Quiet::Unsafe {
            detail: "this host is tainted (cleanup-failed)".to_owned()
        }
    );
    assert_eq!(checks_of(&h, LEASE), 0);
    assert_eq!(restore(&mut h).0, Restored::Done);
}

/// Asserts that a quiet answer is kept: asked again, the agent answers ready without
/// running the checks a second time. It catches a check that runs on every heartbeat and so
/// can taint a host that was found clean (and handed over) on the second look.
#[test]
fn ready_is_kept_and_the_checks_run_once() {
    let mut h = Harness::new("ex-once");
    h.life.apply(&granting(LEASE, vec![]));
    for _ in 0..3 {
        assert_eq!(prepare(&mut h).0, Quiet::Ready);
    }
    assert_eq!(checks_of(&h, LEASE), 1);
}

/// Asserts that a different lease starts over: ready for the first says nothing about the
/// second, which is checked again. It catches a ready flag that outlives its lease.
#[test]
fn a_new_lease_is_not_ready_because_the_last_one_was() {
    let mut h = Harness::new("ex-next");
    h.life.apply(&granting(LEASE, vec![]));
    assert_eq!(prepare(&mut h).0, Quiet::Ready);
    let early = h
        .life
        .apply(&granting("whole-2", vec![start(of("whole-2", "a"))]));
    assert_eq!(
        names(&early),
        [format!("refused a {}", refusal::NO_CAPACITY)]
    );
    h.life.apply(&granting("whole-2", vec![]));
    let result = reply_json(&json!({
        "exclusive": "whole-2",
        "result_acks": [{ "run_id": "a", "ack": "ACCEPTED" }]
    }));
    h.life.apply(&result);
    assert_eq!(prepare(&mut h).0, Quiet::Ready);
    assert_eq!(checks_of(&h, "whole-2"), 1);
}

/// Asserts that with no lease followed the agent is not ready for any (it answers busy and
/// runs no check), so a session that asks before the reply reached the lifecycle waits.
/// It catches a prepare that passes with nothing to prepare for.
#[test]
fn without_a_lease_there_is_nothing_to_be_ready_for() {
    let mut h = Harness::new("ex-none");
    assert_eq!(prepare(&mut h).0, Quiet::Busy);
    assert!(h.log.borrow().is_empty());
}

/// Asserts that a finding of the check taints the host as after any job: the verdict is
/// unsafe with the check's text, the marker is on disk, the taint report offers the check's
/// code, and the lease still cannot start. It catches a refusal that forgets the taint (the
/// next lease would be handed the same host), and a taint that is not written.
#[test]
fn a_finding_taints_the_host_and_refuses_the_lease() {
    let mut h = Harness::with("ex-finding", finding_for("whole-1"));
    h.life.apply(&granting(LEASE, vec![]));
    let (verdict, events) = prepare(&mut h);
    assert_eq!(
        verdict,
        Quiet::Unsafe {
            detail: "pid 9".to_owned()
        }
    );
    assert_eq!(
        names(&events),
        ["tainted - residue", "finding - stray-process"]
    );
    assert!(h.marker().exists());
    let taint = h.life.pending_taint().unwrap();
    assert_eq!(taint.check.as_deref(), Some("stray-process"));
    assert_eq!(taint.run_id, None);
    let late = h.life.apply(&granting(LEASE, vec![start(of(LEASE, "a"))]));
    assert_eq!(
        names(&late),
        [format!("refused a {}", refusal::HOST_TAINTED)]
    );
    // Asked again: still unsafe, and the check does not run a second time.
    let (again, _) = prepare(&mut h);
    assert_eq!(
        again,
        Quiet::Unsafe {
            detail: "this host is tainted (residue)".to_owned()
        }
    );
    assert_eq!(checks_of(&h, LEASE), 1);
}

/// Asserts that a check that cannot run says so in the text, and taints the host as
/// unchecked. It catches a check that could not run being read as clean.
#[test]
fn a_check_that_cannot_run_is_unsafe() {
    let options = Options {
        findings: vec![(
            LEASE,
            CheckFinding::unchecked("proc-unreadable", "no /proc"),
        )],
        ..Options::default()
    };
    let mut h = Harness::with("ex-unchecked", options);
    h.life.apply(&granting(LEASE, vec![]));
    let (verdict, events) = prepare(&mut h);
    assert_eq!(
        verdict,
        Quiet::Unsafe {
            detail: "the check could not run: no /proc".to_owned()
        }
    );
    assert_eq!(names(&events)[0], "tainted - unchecked");
}

/// Asserts that a host already tainted (its own marker, from before) is unsafe without a
/// check: the checks are not run to be told what is known. It catches a tainted host handed
/// over because the checks happened to pass.
#[test]
fn a_host_tainted_before_is_unsafe_without_a_check() {
    let mut h = Harness::with(
        "ex-tainted",
        Options {
            allow_host_jobs: true,
            ..Options::default()
        },
    );
    let mut host = job("h");
    host.isolation = js_agent_wire::Isolation::Host;
    h.start(host);
    h.fake.exit("h", 0);
    h.life.step();
    h.ack("h", "ACCEPTED");
    h.life.apply(&granting(LEASE, vec![]));
    let (verdict, _) = prepare(&mut h);
    assert_eq!(
        verdict,
        Quiet::Unsafe {
            detail: "this host is tainted (host-job)".to_owned()
        }
    );
    assert_eq!(checks_of(&h, LEASE), 0);
}

/// Asserts that a marker that cannot be written still refuses the lease: the host is
/// tainted in memory, the verdict is unsafe, and the write failure is a finding beside the
/// check's. It catches a finding that is lost when the marker is unwritable.
#[test]
fn a_finding_is_unsafe_even_when_the_marker_cannot_be_written() {
    let mut h = Harness::with("ex-nowrite", finding_for("whole-1"));
    std::fs::create_dir(
        h.marker()
            .with_file_name(js_agent::lifecycle::taint::MARKER_TEMP_FILE),
    )
    .unwrap();
    h.life.apply(&granting(LEASE, vec![]));
    let (verdict, events) = prepare(&mut h);
    assert!(matches!(verdict, Quiet::Unsafe { .. }));
    assert!(h.life.is_tainted());
    assert!(
        h.life.pending_taint().is_none(),
        "not on disk: not reported"
    );
    let codes: Vec<&str> = events
        .iter()
        .filter_map(|event| match event {
            Event::Finding(finding) => Some(finding.code),
            _ => None,
        })
        .collect();
    assert_eq!(codes, [code::TAINT_WRITE_FAILED, "stray-process"]);
}

/// Asserts that restoring waits for the runs the lease left to be gone, runs the checks
/// once with the name `restore`, and is done: a run still held is busy and runs no check.
/// It catches a restore that checks a host with a job on it, and one that never finishes.
#[test]
fn restoring_waits_for_the_runs_then_checks_once() {
    let mut h = Harness::new("ex-restore");
    h.life.apply(&granting(LEASE, vec![]));
    assert_eq!(prepare(&mut h).0, Quiet::Ready);
    h.life.apply(&granting(LEASE, vec![start(of(LEASE, "a"))]));
    h.life.apply(&reply(vec![Command::Stop {
        run_id: "a".to_owned(),
    }]));
    assert_eq!(h.life.exclusive(), None);
    assert_eq!(restore(&mut h).0, Restored::Busy, "the run is not gone yet");
    assert_eq!(checks_of(&h, "restore"), 0);
    h.life.step();
    h.ack("a", "ACCEPTED");
    let (done, events) = restore(&mut h);
    assert_eq!((done, events.is_empty()), (Restored::Done, true));
    assert_eq!(checks_of(&h, "restore"), 1);
    assert!(!h.life.is_tainted());
}

/// Asserts that a finding when the lease is restored taints the host, and restoring is
/// still done (the agent returns to serving with the taint reported). It catches a restore
/// that drops the finding, and one that waits for ever on a host it has marked.
#[test]
fn a_finding_at_restore_taints_the_host() {
    let mut h = Harness::with("ex-restore-finding", finding_for("restore"));
    let (done, events) = restore(&mut h);
    assert_eq!(done, Restored::Done);
    assert_eq!(
        names(&events),
        ["tainted - residue", "finding - stray-process"]
    );
    assert!(h.life.pending_taint().is_some());
}

/// Asserts the restart rule: a run taken over at start that belongs to the lease the first
/// reply names proves the host was ready, so the hold starts ready. The consumer's own new
/// start is admitted at once, and `prepare` answers ready without a run being quiet. It
/// catches a restart that forgets a ready lease and refuses the consumer's starts.
#[test]
fn a_restart_while_ready_keeps_the_lease_ready() {
    let mut h = Harness::new("ex-restart-ready");
    h.start(of(LEASE, "a"));
    h.restart().unwrap();
    let events = h.life.apply(&granting(LEASE, vec![start(of(LEASE, "b"))]));
    assert_eq!(names(&events), ["started b"]);
    assert_eq!(prepare(&mut h).0, Quiet::Ready);
}

/// Asserts that runs taken over at start that belong to another lease than the one named
/// prove nothing: the hold is prepared for as for any grant, the lease's own start is
/// refused, and the host is busy until those runs are gone. It catches a rule that treats
/// any adopted run as proof.
#[test]
fn a_restart_with_the_runs_of_another_lease_prepares_as_usual() {
    let mut h = Harness::new("ex-restart-other");
    h.start(of("other", "a"));
    h.restart().unwrap();
    let events = h.life.apply(&granting(LEASE, vec![start(of(LEASE, "b"))]));
    assert_eq!(
        names(&events),
        [format!("refused b {}", refusal::NO_CAPACITY)]
    );
    assert_eq!(prepare(&mut h).0, Quiet::Busy);
}

/// Asserts that a first reply naming no lease does not spend the proof: the grant that
/// follows it names the adopted lease, the hold starts ready, the consumer's start is
/// admitted and nothing is refused. It catches adopted leases dropped by a reply that
/// names none, which loses the ready mark when a reply and a grant cross.
#[test]
fn a_reply_without_exclusive_keeps_the_restart_proof() {
    let mut h = Harness::new("ex-restart-crossed");
    h.start(of(LEASE, "a"));
    h.restart().unwrap();
    h.life.apply(&reply(Vec::new()));
    let events = h.life.apply(&granting(LEASE, vec![start(of(LEASE, "b"))]));
    assert_eq!(names(&events), ["started b"]);
    assert_eq!(prepare(&mut h).0, Quiet::Ready);
}

/// Asserts that the proof is spent by the first reply that names a lease: the lease
/// ends, is granted again later, and is prepared for although a run of it is held. It
/// catches adopted leases that stay trusted for the life of the process.
#[test]
fn the_restart_rule_applies_to_the_first_named_lease_only() {
    let mut h = Harness::new("ex-restart-first");
    h.start(of(LEASE, "a"));
    h.restart().unwrap();
    h.life.apply(&granting(LEASE, Vec::new()));
    h.life.apply(&reply(Vec::new()));
    h.life.apply(&granting(LEASE, Vec::new()));
    assert_eq!(prepare(&mut h).0, Quiet::Busy);
}

/// Asserts that the quiet check answers only for the lease the host is held for: asked
/// about another lease (a lagging question about one that ended, or about one not yet
/// granted) it is busy and runs no check, and the host is not marked ready by it; asked
/// about the held lease it still finds the host quiet. It catches a host that answers for
/// whichever lease is asked, which would report the new lease ready on the old one's
/// verdict.
#[test]
fn the_quiet_check_answers_only_for_the_held_lease() {
    let mut h = Harness::new("ex-other-lease");
    h.life.apply(&granting(LEASE, Vec::new()));
    let (verdict, events) = {
        let mut events = Vec::new();
        (h.life.prepare("whole-2", &mut events), events)
    };
    assert_eq!(verdict, Quiet::Busy);
    assert!(events.is_empty());
    assert_eq!(checks_of(&h, LEASE), 0, "no check ran for another lease");
    assert_eq!(prepare(&mut h).0, Quiet::Ready);
}
