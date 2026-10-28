//! The session through a whole-node lease: the phases and reports of its heartbeats, from
//! the reply that grants the lease to the one that ends it. The server is a script, the
//! host is either a script of answers or the real lifecycle over a fake runtime, and the
//! clock is the lifecycle tests' manual one, so nothing here waits and no test depends on
//! how long anything took.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::time::Duration;

use js_agent::lifecycle::{Quiet, Restored};
use js_agent::{Answer, HEARTBEAT_PATH, Note, PREPARE_TIMEOUT, Quiesce, Session, Step, Transport};
use js_agent_wire::{Command, HeartbeatReply, RefusalReason, refusal};
use serde_json::{Value, json};

use crate::lifecycle::fake::ManualClock;
use crate::lifecycle::{Harness, Options, job};
use crate::support::{Reply, Script, Sent, beat_ok, hello, lowest, reply, welcome};

pub const LEASE: &str = "whole-1";

/// How long the session waits for a run it did not know of, in these tests.
pub const TIMEOUT: Duration = Duration::from_secs(60);

/// A heartbeat reply that names `lease` (or none) and carries `commands`.
fn granted(lease: Option<&str>, commands: Vec<Command>) -> Reply {
    let body = HeartbeatReply {
        commands,
        exclusive: lease.map(str::to_owned),
        ..HeartbeatReply::default()
    };
    reply(200, serde_json::to_value(body).unwrap())
}

pub fn names(lease: &str) -> Reply {
    granted(Some(lease), Vec::new())
}

fn silent() -> Reply {
    granted(None, Vec::new())
}

/// What one heartbeat says about the lease, as a line: the phase, then `ready=<lease>` or
/// `refused=<code>:<detail>` when it carries them.
fn line(body: &Value) -> String {
    let mut text = body["phase"].as_str().unwrap().to_owned();
    if let Some(lease) = body.get("exclusive_ready") {
        text += &format!(" ready={}", lease.as_str().unwrap());
    }
    if let Some(refused) = body.get("exclusive_refused") {
        text += &format!(
            " refused={}:{}",
            refused["code"].as_str().unwrap(),
            refused["detail"].as_str().unwrap()
        );
    }
    text
}

/// The line of each heartbeat sent so far.
pub fn lines(sent: &Sent) -> Vec<String> {
    let sent = sent.borrow();
    let beats = sent.iter().filter(|(path, _)| path == HEARTBEAT_PATH);
    beats.map(|(_, body)| line(body)).collect()
}

pub fn note(step: &Step) -> Option<Note> {
    match step {
        Step::Wait { note, .. } => note.clone(),
        Step::Fatal(fatal) => panic!("fatal: {fatal}"),
        Step::Failed(why) => panic!("failed: {why}"),
    }
}

/// What the session asked of a scripted host.
#[derive(Default)]
pub struct Asked {
    pub prepared: usize,
    pub restored: usize,
}

/// A host that answers from lists (busy when a list is used up) and counts the questions.
pub struct Probe {
    prepare: VecDeque<Quiet>,
    restore: VecDeque<Restored>,
    asked: Rc<RefCell<Asked>>,
}

impl Probe {
    pub fn new(prepare: Vec<Quiet>, restore: Vec<Restored>) -> (Self, Rc<RefCell<Asked>>) {
        let asked = Rc::new(RefCell::new(Asked::default()));
        let probe = Self {
            prepare: prepare.into(),
            restore: restore.into(),
            asked: Rc::clone(&asked),
        };
        (probe, asked)
    }
}

impl Quiesce for Probe {
    fn prepare(&mut self) -> Quiet {
        self.asked.borrow_mut().prepared += 1;
        self.prepare.pop_front().unwrap_or(Quiet::Busy)
    }

    fn restore(&mut self) -> Restored {
        self.asked.borrow_mut().restored += 1;
        self.restore.pop_front().unwrap_or(Restored::Busy)
    }
}

pub fn unsafe_host(detail: &str) -> Quiet {
    Quiet::Unsafe {
        detail: detail.to_owned(),
    }
}

/// The lifecycle as the session's host, applying each reply the transport carried the way
/// the worker of a session that runs jobs does.
struct Tap {
    inner: Script,
    harness: Rc<RefCell<Harness>>,
}

impl Transport for Tap {
    fn post(&mut self, path: &str, body: &Value) -> Result<Answer, String> {
        let answer = self.inner.post(path, body)?;
        let decoded = serde_json::from_value::<HeartbeatReply>(answer.body.clone());
        if let (HEARTBEAT_PATH, 200, Ok(decoded)) = (path, answer.status, decoded) {
            self.harness.borrow_mut().life.apply(&decoded);
        }
        Ok(answer)
    }
}

struct Wired(Rc<RefCell<Harness>>);

impl Quiesce for Wired {
    fn prepare(&mut self) -> Quiet {
        self.0.borrow_mut().life.prepare(&mut Vec::new())
    }

    fn restore(&mut self) -> Restored {
        self.0.borrow_mut().life.restore(&mut Vec::new())
    }
}

/// Acknowledges the results of `runs`, in a reply that keeps naming the lease.
fn ack_all(harness: &Rc<RefCell<Harness>>, runs: &[&str]) {
    let acks: Vec<Value> = runs
        .iter()
        .map(|run| json!({ "run_id": run, "ack": "ACCEPTED" }))
        .collect();
    let body = json!({ "exclusive": LEASE, "result_acks": acks });
    let ack: HeartbeatReply = serde_json::from_value(body).unwrap();
    harness.borrow_mut().life.apply(&ack);
}

/// Asserts the whole lease through the real lifecycle over a fake runtime. A run the
/// scheduler did not know of is running when the lease is granted, and a start for another
/// lease arrives with the grant. Then: the first beat after the grant says `preparing`; no
/// beat reports ready while any run is held (running, or exited with its result owed);
/// the first beat after the last result is acknowledged reports ready and says `serving`;
/// the report is resent until a reply stops naming the lease; then one beat says
/// `restoring` and the next `serving` with nothing reported. The start for another lease
/// ran nothing and was answered `failed_to_start` with `no-capacity`. It catches a ready
/// reported early or never, a missing phase, a report that is not resent, a restore that
/// never ends, and a gate that let the other lease's start through.
#[test]
fn the_whole_sequence_over_the_real_lifecycle() {
    let harness = Rc::new(RefCell::new(Harness::new("sess-sequence")));
    harness.borrow_mut().start(job("a"));
    let other = Command::Start(Box::new(job("b")));
    let replies = vec![
        welcome(1),
        granted(Some(LEASE), vec![other]),
        names(LEASE),
        names(LEASE),
        names(LEASE),
        names(LEASE),
        silent(),
        silent(),
        silent(),
        silent(),
    ];
    let (script, sent) = Script::new(replies);
    let mut tap = Tap {
        inner: script,
        harness: Rc::clone(&harness),
    };
    let mut wired = Wired(Rc::clone(&harness));
    let clock = harness.borrow().clock.clone();
    let mut session =
        Session::new(hello("h1"), &mut tap, lowest()).with_quiesce(&mut wired, &clock, TIMEOUT);

    session.tick(); // hello
    let granting = session.tick();
    assert_eq!(
        note(&granting),
        Some(Note::Preparing {
            lease: LEASE.to_owned()
        })
    );
    {
        let h = harness.borrow();
        assert_eq!(h.fake.created().len(), 1, "only the run that was running");
        let results = h.life.results();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].run_id, "b");
        assert_eq!(results[0].detail.as_deref(), Some(refusal::NO_CAPACITY));
    }
    session.tick(); // announced
    session.tick(); // asks: a runs
    harness.borrow().fake.exit("a", 0);
    harness.borrow_mut().life.step();
    session.tick(); // asks: a's result is owed
    ack_all(&harness, &["a", "b"]);
    session.tick(); // asks: quiet
    session.tick(); // resent
    session.tick(); // the reply before named nothing: restoring (the beat before it ended)
    session.tick(); // asks: done
    session.tick();

    assert_eq!(
        lines(&sent),
        [
            "serving",
            "preparing",
            "preparing",
            "preparing",
            "serving ready=whole-1",
            "serving ready=whole-1",
            "restoring",
            "serving",
            "serving",
        ]
    );
}

/// Asserts that a finding of the post-job check is reported as `exclusive_refused` with the
/// code `host-unsafe` and the check's text on the beat that asked, and again on every beat
/// until the reply stops naming the lease, with the phase still `preparing` and never a
/// ready; that the lifecycle has the taint to report (the existing taint report) for the
/// check's code; and that the session notes the refusal once. It catches a finding that
/// still reports ready, one that is reported once and then forgotten, and one that does
/// not taint the host.
#[test]
fn a_finding_is_refused_as_host_unsafe_and_taints_the_host() {
    let options = Options {
        findings: vec![(
            LEASE,
            js_agent::lifecycle::CheckFinding::residue("stray-process", "pid 9"),
        )],
        ..Options::default()
    };
    let harness = Rc::new(RefCell::new(Harness::with("sess-finding", options)));
    let replies = vec![
        welcome(1),
        names(LEASE),
        names(LEASE),
        names(LEASE),
        silent(),
        silent(),
        silent(),
    ];
    let (script, sent) = Script::new(replies);
    let mut tap = Tap {
        inner: script,
        harness: Rc::clone(&harness),
    };
    let mut wired = Wired(Rc::clone(&harness));
    let clock = harness.borrow().clock.clone();
    let mut session =
        Session::new(hello("h1"), &mut tap, lowest()).with_quiesce(&mut wired, &clock, TIMEOUT);
    let steps: Vec<Step> = (0..8).map(|_| session.tick()).collect();

    let refused = "preparing refused=host-unsafe:pid 9";
    assert_eq!(
        lines(&sent),
        [
            "serving",
            "preparing",
            refused,
            refused,
            "restoring",
            "serving",
            "serving",
        ]
    );
    let taint = harness.borrow().life.pending_taint().unwrap();
    assert_eq!(taint.check.as_deref(), Some("stray-process"));
    let notes: Vec<Option<Note>> = steps.iter().map(note).collect();
    assert_eq!(
        notes[3],
        Some(Note::Refused {
            code: RefusalReason::HostUnsafe
        })
    );
    assert_eq!(
        notes
            .iter()
            .filter(|n| matches!(n, Some(Note::Refused { .. })))
            .count(),
        1
    );
}

/// Asserts the local deadline with the injected clock: counted from the reply that granted
/// the lease, a host still busy one millisecond before `prepare_timeout` is still
/// `preparing` and reports nothing, and at the timeout the next beat reports
/// `exclusive_refused` with `not-quiet`, resent after without asking the host again. No
/// wall-clock time passes. It catches a deadline counted from the wrong event, an off-by-one
/// at the boundary, and a host that is asked again after the refusal.
#[test]
fn a_run_still_held_at_prepare_timeout_is_refused_as_not_quiet() {
    let clock = ManualClock::default();
    clock.advance(1_000);
    let replies = vec![
        welcome(1),
        names(LEASE),
        names(LEASE),
        names(LEASE),
        names(LEASE),
        names(LEASE),
    ];
    let (mut script, sent) = Script::new(replies);
    let (mut probe, asked) = Probe::new(Vec::new(), Vec::new());
    let mut session =
        Session::new(hello("h1"), &mut script, lowest()).with_quiesce(&mut probe, &clock, TIMEOUT);
    session.tick(); // hello
    session.tick(); // the grant, at 1_000 ms
    session.tick(); // announced
    clock.advance(59_999);
    session.tick(); // asks at 60_999: busy
    clock.advance(1);
    let refusing = session.tick(); // asks at 61_000: busy, and the deadline
    session.tick();
    assert_eq!(asked.borrow().prepared, 2);
    let detail = "a run was still held when prepare_timeout passed";
    let refused = format!("preparing refused=not-quiet:{detail}");
    assert_eq!(
        lines(&sent),
        [
            "serving",
            "preparing",
            "preparing",
            refused.as_str(),
            refused.as_str()
        ]
    );
    assert_eq!(
        note(&refusing),
        Some(Note::Refused {
            code: RefusalReason::NotQuiet
        })
    );
}

/// Asserts that the default `prepare_timeout` is the pool's ten minutes. It catches a
/// default changed without the design.
#[test]
fn the_default_prepare_timeout_is_ten_minutes() {
    assert_eq!(PREPARE_TIMEOUT, Duration::from_secs(600));
    let clock = ManualClock::default();
    let (mut script, sent) = Script::new(vec![
        welcome(1),
        names(LEASE),
        names(LEASE),
        names(LEASE),
        names(LEASE),
    ]);
    let (mut probe, _) = Probe::new(Vec::new(), Vec::new());
    let mut session =
        Session::new(hello("h1"), &mut script, lowest()).with_default_quiesce(&mut probe, &clock);
    session.tick(); // hello
    session.tick(); // the grant, at 0 ms
    session.tick(); // announced
    clock.advance(599_999);
    session.tick(); // busy, one millisecond short
    clock.advance(1);
    session.tick(); // busy, at the timeout
    let detail = "a run was still held when prepare_timeout passed";
    let refused = format!("preparing refused=not-quiet:{detail}");
    assert_eq!(
        lines(&sent),
        ["serving", "preparing", "preparing", refused.as_str()]
    );
}

/// Asserts that a session with no host to ask leaves everything as it was: a reply that
/// names a lease changes no phase and reports nothing. It catches `exclusive` acted on by
/// an agent that was not given the means to be ready.
#[test]
fn without_a_host_the_lease_is_ignored() {
    let (mut script, sent) = Script::new(vec![welcome(1), names(LEASE), names(LEASE), silent()]);
    let mut session = Session::new(hello("h1"), &mut script, lowest());
    let steps: Vec<Step> = (0..4).map(|_| session.tick()).collect();
    assert_eq!(lines(&sent), ["serving", "serving", "serving"]);
    assert!(steps[1..].iter().all(|step| note(step).is_none()));
}

/// Asserts that a lease id the wire refuses starts nothing and ends nothing: the session
/// goes on as it was, before a grant and during one. It catches a bad `exclusive` read as
/// "no lease", which would drop a lease that is held.
#[test]
fn an_unreadable_lease_id_changes_nothing() {
    let (mut script, sent) = Script::new(vec![
        welcome(1),
        names("not a lease"),
        names(LEASE),
        names("not a lease"),
        names(LEASE),
        beat_ok(),
    ]);
    let (mut probe, _) = Probe::new(vec![Quiet::Ready], Vec::new());
    let clock = ManualClock::default();
    let mut session =
        Session::new(hello("h1"), &mut script, lowest()).with_quiesce(&mut probe, &clock, TIMEOUT);
    for _ in 0..6 {
        session.tick();
    }
    // Beat 1 follows nothing (bad id), beat 2 announces, beat 3 is ready and the bad id
    // that comes after it ends nothing.
    assert_eq!(
        lines(&sent),
        [
            "serving",
            "serving",
            "preparing",
            "serving ready=whole-1",
            "serving ready=whole-1",
        ]
    );
}

/// Asserts that a reply that names another lease starts the preparation over for it: the
/// new lease is announced again, asked again, and its deadline counts from its own reply.
/// It catches a ready or a deadline that outlives the lease it was for.
#[test]
fn another_lease_starts_over() {
    let (mut script, sent) = Script::new(vec![
        welcome(1),
        names(LEASE),
        names(LEASE),
        names("whole-2"),
        names("whole-2"),
        names("whole-2"),
    ]);
    let (mut probe, asked) = Probe::new(vec![Quiet::Ready, Quiet::Ready], Vec::new());
    let clock = ManualClock::default();
    let mut session =
        Session::new(hello("h1"), &mut script, lowest()).with_quiesce(&mut probe, &clock, TIMEOUT);
    let steps: Vec<Step> = (0..6).map(|_| session.tick()).collect();
    assert_eq!(
        lines(&sent),
        [
            "serving",
            "preparing",
            "serving ready=whole-1",
            "preparing",
            "serving ready=whole-2",
        ]
    );
    assert_eq!(asked.borrow().prepared, 2);
    assert_eq!(
        note(&steps[3]),
        Some(Note::Preparing {
            lease: "whole-2".to_owned()
        })
    );
}

/// Asserts that a lease granted again while the host is restoring goes back to preparing,
/// and that a restore that is still busy keeps announcing `restoring` and is asked again on
/// each beat until it is done. It catches a restore that is asked once, and a grant that is
/// lost because the host was busy restoring.
#[test]
fn restoring_is_asked_until_done_and_a_new_grant_interrupts_it() {
    let (mut script, sent) = Script::new(vec![
        welcome(1),
        names(LEASE),
        names(LEASE),
        names(LEASE),
        silent(),
        silent(),
        silent(),
        names("whole-2"),
        names("whole-2"),
    ]);
    let (mut probe, asked) = Probe::new(vec![Quiet::Ready], vec![Restored::Busy, Restored::Busy]);
    let clock = ManualClock::default();
    let mut session =
        Session::new(hello("h1"), &mut script, lowest()).with_quiesce(&mut probe, &clock, TIMEOUT);
    for _ in 0..9 {
        session.tick();
    }
    assert_eq!(
        lines(&sent),
        [
            "serving",
            "preparing",
            "serving ready=whole-1",
            "serving ready=whole-1",
            "restoring",
            "restoring",
            "restoring",
            "preparing",
        ]
    );
    assert_eq!(asked.borrow().restored, 2);
}

/// Asserts that a drain is not overridden by the lease: the last heartbeat says
/// `draining` and still carries the ready report. It catches a lease that reverts the phase
/// of a host that is stopping to `serving`.
#[test]
fn a_drain_is_not_overridden_by_the_lease() {
    let (mut script, sent) =
        Script::new(vec![welcome(1), names(LEASE), names(LEASE), names(LEASE)]);
    let (mut probe, _) = Probe::new(vec![Quiet::Ready], Vec::new());
    let clock = ManualClock::default();
    let mut session =
        Session::new(hello("h1"), &mut script, lowest()).with_quiesce(&mut probe, &clock, TIMEOUT);
    for _ in 0..4 {
        session.tick();
    }
    session.drain().unwrap();
    let sent = sent.borrow();
    assert_eq!(
        line(&sent.last().unwrap().1),
        "draining ready=whole-1",
        "the phase of the drain, and the report"
    );
}

/// Asserts that a drain does not ask the host or move the lease: with the lease still
/// preparing, the drain beat carries nothing of it and the host is not asked; with a
/// refusal pending, the drain beat carries it and the stage keeps it for the next beat.
/// It catches a drain that runs the lifecycle's checks during shutdown, or consumes a
/// refusal or stage announcement on a beat that may be lost.
#[test]
fn a_drain_does_not_ask_the_host_or_move_the_lease() {
    let (mut script, sent) = Script::new(vec![welcome(1), names(LEASE), names(LEASE)]);
    let (mut probe, asked) = Probe::new(vec![Quiet::Ready], Vec::new());
    let clock = ManualClock::default();
    let mut session =
        Session::new(hello("h1"), &mut script, lowest()).with_quiesce(&mut probe, &clock, TIMEOUT);
    for _ in 0..3 {
        session.tick();
    }
    assert_eq!(lines(&sent), ["serving", "preparing"]);
    session.drain().unwrap();
    assert_eq!(asked.borrow().prepared, 0, "the host was not asked");
    assert_eq!(line(&sent.borrow().last().unwrap().1), "draining");

    let (mut script, sent) =
        Script::new(vec![welcome(1), names(LEASE), names(LEASE), names(LEASE)]);
    let (mut probe, asked) = Probe::new(vec![unsafe_host("bad")], Vec::new());
    let mut session =
        Session::new(hello("h1"), &mut script, lowest()).with_quiesce(&mut probe, &clock, TIMEOUT);
    for _ in 0..4 {
        session.tick();
    }
    let before = asked.borrow().prepared;
    session.drain().unwrap();
    assert_eq!(asked.borrow().prepared, before);
    let got = lines(&sent);
    assert!(
        got.last().unwrap().starts_with("draining refused="),
        "{got:?}"
    );
}

/// Asserts that a refusal always fits the wire: a finding's text with a control character
/// is replaced by a fixed text (the heartbeat is still sent, with the code), a text over
/// 256 bytes is cut at a character boundary and sent, and one that fits is sent as it is.
/// It catches a heartbeat the wire would refuse, which the agent would then retry for ever
/// while the scheduler waits on the deadline.
#[test]
fn a_refusal_detail_always_fits_the_wire() {
    let long = "é".repeat(200); // 400 bytes
    let cases = [
        (
            "line\nbreak",
            "the host is not safe to hand over".to_owned(),
        ),
        (long.as_str(), "é".repeat(128)),
        ("fits", "fits".to_owned()),
    ];
    for (given, sent_as) in cases {
        let (mut script, sent) =
            Script::new(vec![welcome(1), names(LEASE), names(LEASE), names(LEASE)]);
        let (mut probe, _) = Probe::new(vec![unsafe_host(given)], Vec::new());
        let clock = ManualClock::default();
        let mut session = Session::new(hello("h1"), &mut script, lowest())
            .with_quiesce(&mut probe, &clock, TIMEOUT);
        let steps: Vec<Step> = (0..4).map(|_| session.tick()).collect();
        assert_eq!(
            lines(&sent)[2],
            format!("preparing refused=host-unsafe:{sent_as}")
        );
        assert!(
            steps
                .iter()
                .all(|step| !matches!(note(step), Some(Note::Retrying { .. }))),
            "every heartbeat was sendable"
        );
    }
}

/// Asserts that the report survives the loss of the session: the server says it does not
/// know the epoch, the agent says hello again, and the heartbeat after still reports ready
/// for the lease that has not ended. It catches a lease forgotten with the session.
#[test]
fn the_report_survives_a_new_session() {
    let (mut script, sent) = Script::new(vec![
        welcome(1),
        names(LEASE),
        names(LEASE),
        crate::support::refused(409, refusal::STALE_EPOCH),
        welcome(2),
        names(LEASE),
    ]);
    let (mut probe, asked) = Probe::new(vec![Quiet::Ready], Vec::new());
    let clock = ManualClock::default();
    let mut session =
        Session::new(hello("h1"), &mut script, lowest()).with_quiesce(&mut probe, &clock, TIMEOUT);
    for _ in 0..6 {
        session.tick();
    }
    assert_eq!(
        lines(&sent),
        [
            "serving",
            "preparing",
            "serving ready=whole-1",
            "serving ready=whole-1",
        ]
    );
    assert_eq!(asked.borrow().prepared, 1);
}

/// Asserts the texts of the notes the lease adds. It catches a note that prints the
/// session secret or says nothing.
#[test]
fn the_notes_read_plainly() {
    let preparing = Note::Preparing {
        lease: LEASE.to_owned(),
    };
    let refused = Note::Refused {
        code: RefusalReason::NotQuiet,
    };
    assert_eq!(
        preparing.to_string(),
        "preparing the host for lease whole-1"
    );
    assert_eq!(
        refused.to_string(),
        "refusing the whole-node lease: not-quiet"
    );
    assert_eq!(
        Note::Restoring.to_string(),
        "the whole-node lease ended: restoring the host"
    );
}

/// Asserts the restart rule through the session: the agent restarted while the lease was
/// ready, with the consumer's run adopted and the first reply naming the lease. The consumer's
/// new start for the lease is admitted, the beat after the announcement reports ready, and
/// no refusal is sent even after `prepare_timeout` has passed on the clock. It catches a
/// restart that leaves the host `preparing` with a run it cannot quiet, then refuses a lease
/// that is in use as `not-quiet`.
#[test]
fn a_restart_while_ready_reports_ready_and_never_refuses() {
    let harness = Rc::new(RefCell::new(Harness::new("sess-restart-ready")));
    let own = |run: &str| {
        let mut spec = job(run);
        spec.lease_id = LEASE.to_owned();
        spec
    };
    harness.borrow_mut().start(own("a"));
    harness.borrow_mut().restart().unwrap();
    let replies = vec![
        welcome(1),
        granted(Some(LEASE), vec![Command::Start(Box::new(own("b")))]),
        names(LEASE),
        names(LEASE),
        names(LEASE),
    ];
    let (script, sent) = Script::new(replies);
    let mut tap = Tap {
        inner: script,
        harness: Rc::clone(&harness),
    };
    let mut wired = Wired(Rc::clone(&harness));
    let clock = harness.borrow().clock.clone();
    let mut session =
        Session::new(hello("h1"), &mut tap, lowest()).with_quiesce(&mut wired, &clock, TIMEOUT);
    session.tick(); // hello
    session.tick(); // the grant, with the consumer's start
    assert_eq!(harness.borrow().fake.created().len(), 2, "b was admitted");
    clock.advance(TIMEOUT.as_millis() as u64 + 1);
    session.tick(); // announced
    session.tick(); // asks: ready
    session.tick();
    assert_eq!(
        lines(&sent),
        [
            "serving",
            "preparing",
            "serving ready=whole-1",
            "serving ready=whole-1"
        ]
    );
}
