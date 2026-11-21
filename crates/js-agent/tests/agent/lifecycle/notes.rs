//! What a post-job check notes without tainting the host (`CheckNote`), the detail a taint
//! keeps of what changed, and the checks an older agent tainted for, which now clear. The
//! runtime and the clock are the in-memory ones; the marker is a real file in the state root.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use js_agent::integrity::code as check;
use js_agent::lifecycle::taint::reason;
use js_agent::lifecycle::{
    CHECK_DETAIL_MAX, CheckFinding, CheckHook, CheckNote, Event, Finished, Quiet, Restored,
};
use js_agent_wire::{HeartbeatReply, Isolation, Taint};
use serde_json::json;

use super::{Harness, Options, cleanup, job, names, reply, reply_json};

/// The wait between re-checks when `--taint-recheck-ms` is not given, in milliseconds.
const EVERY: u64 = 60_000;

/// What one call of the check does: the notes it leaves, and what it finds.
#[derive(Default)]
struct Call {
    notes: Vec<CheckNote>,
    found: Option<CheckFinding>,
}

/// The calls of a [`Noting`] check, in order; a call past the script passes and notes nothing.
type Script = Rc<RefCell<VecDeque<Call>>>;

/// A check that plays a script. Each call replaces what it noted before, so a note the
/// lifecycle did not take at its call is lost.
struct Noting {
    script: Script,
    pending: Vec<CheckNote>,
}

impl Noting {
    fn of(calls: Vec<Call>) -> (Box<dyn CheckHook>, Script) {
        let script: Script = Rc::new(RefCell::new(calls.into()));
        let hook = Self {
            script: script.clone(),
            pending: Vec::new(),
        };
        (Box::new(hook), script)
    }
}

impl CheckHook for Noting {
    fn run(&mut self, _job: &Finished<'_>) -> Result<(), CheckFinding> {
        let call = self.script.borrow_mut().pop_front().unwrap_or_default();
        self.pending = call.notes;
        call.found.map_or(Ok(()), Err)
    }

    fn notes(&mut self) -> Vec<CheckNote> {
        std::mem::take(&mut self.pending)
    }
}

fn note(code: &'static str, detail: &str) -> CheckNote {
    CheckNote::new(code, detail)
}

fn residue() -> CheckFinding {
    CheckFinding::residue(check::PROCESS_AS_JOB_USER, "pid 41")
}

/// A harness whose check plays `calls`.
fn noting(name: &str, calls: Vec<Call>) -> (Harness, Script) {
    let (hook, script) = Noting::of(calls);
    let options = Options {
        hook: Some(hook),
        ..Options::default()
    };
    (Harness::with(name, options), script)
}

/// The server acknowledges the report of the taint.
fn ack(h: &mut Harness) -> Vec<Event> {
    h.life.apply(&reply_json(&json!({ "taint_ack": true })))
}

/// The server acknowledges the report of the clear.
fn ack_clear(h: &mut Harness) -> Vec<Event> {
    h.life
        .apply(&reply_json(&json!({ "taint_cleared_ack": true })))
}

/// Moves the clock by `ms`, steps and returns the events of the step.
fn tick(h: &mut Harness, ms: u64) -> Vec<Event> {
    h.clock.advance(ms);
    h.life.step()
}

/// The finding among `events` of `code`: its run and its message.
fn finding_of(events: &[Event], code: &str) -> (Option<String>, String) {
    let found = events.iter().find_map(|event| match event {
        Event::Finding(finding) if finding.code == code => Some(finding),
        _ => None,
    });
    let finding = found.unwrap_or_else(|| panic!("no finding {code}: {events:?}"));
    (finding.run_id.clone(), finding.message.clone())
}

/// The detail of the taint the host got in `events`.
fn tainted_detail(events: &[Event]) -> Option<String> {
    events.iter().find_map(|event| match event {
        Event::Tainted { detail, .. } => Some(detail.clone()),
        _ => None,
    })?
}

/// Asserts that a note a check leaves after a job is one finding of the job, with the note's
/// code and detail, logged between the removal of the scratch directory and the passed
/// checks, and that it taints nothing: the checks pass, the slot is freed, the host takes the
/// next job, and a run whose check notes nothing logs no finding. It catches a note that is
/// dropped, one that taints the host or blocks the slot, and a note that is logged again for
/// the next job.
#[test]
fn a_note_after_a_job_is_logged_and_the_job_passes() {
    let notes = vec![note(
        check::MODULE_CHANGED,
        "loaded inet_diag, unloaded ext4",
    )];
    let (mut h, _) = noting("note-passes", vec![Call { notes, found: None }]);
    h.start(job("r1"));
    h.fake.exit("r1", 0);
    let events = h.life.step();
    assert_eq!(
        names(&events),
        [
            "result r1",
            "container removed r1",
            "scratch removed r1",
            "finding r1 kernel-module-changed",
            "checks passed r1",
            "freed r1",
        ]
    );
    assert_eq!(
        finding_of(&events, check::MODULE_CHANGED),
        (
            Some("r1".to_owned()),
            "loaded inet_diag, unloaded ext4".to_owned()
        )
    );
    assert!(!h.life.is_tainted());
    assert_eq!(h.life.pending_taint(), None);
    assert_eq!(h.life.quarantined_slots(), Vec::<u32>::new());
    assert_eq!(h.life.free().cpu_millis, 4000);

    assert_eq!(names(&h.start(job("r2"))), ["started r2"]);
    h.fake.exit("r2", 0);
    let events = names(&h.life.step());
    assert_eq!(
        events,
        [vec!["result r2".to_owned()], cleanup("r2")].concat()
    );
}

/// Asserts that notes beside a finding are all logged, in the order of the check's own calls
/// (the notes first), and that the finding taints and blocks the run as it does without them:
/// the host is tainted for the finding's code with its detail, the slot is quarantined, and
/// no note is a taint. It catches notes that are lost when the check also finds something,
/// and a finding whose taint takes a note's code.
#[test]
fn a_note_beside_a_finding_is_logged_and_the_finding_still_taints() {
    let notes = vec![
        note(check::GROUP_CHANGED, "added docker"),
        note(check::NEW_MOUNT, "/snap/core/1 (squashfs) from /dev/loop1"),
    ];
    let found = Some(residue());
    let (mut h, _) = noting("note-and-finding", vec![Call { notes, found }]);
    h.start(job("r1"));
    h.fake.exit("r1", 0);
    let events = h.life.step();
    assert_eq!(
        names(&events),
        [
            "result r1",
            "container removed r1",
            "scratch removed r1",
            "finding r1 group-changed",
            "finding r1 new-mount",
            "tainted r1 residue",
            "finding r1 process-as-job-user",
        ]
    );
    assert_eq!(tainted_detail(&events), Some("pid 41".to_owned()));
    let taint = h.life.pending_taint().unwrap();
    assert_eq!(taint.check.as_deref(), Some(check::PROCESS_AS_JOB_USER));
    assert_eq!(h.life.quarantined_slots(), [0]);
    assert_eq!(h.life.taint_detail(), Some("pid 41".to_owned()));
}

/// Asserts that the check of a whole-node lease logs its notes as findings of no run, for the
/// quiet check that makes the host ready and for the check after the lease, and that neither
/// taints the host or changes the answer. It catches an exclusive check that swallows the
/// notes, and one that reads a note as a finding.
#[test]
fn a_note_at_the_check_of_a_whole_node_lease_is_logged_and_taints_nothing() {
    let calls = vec![
        Call {
            notes: vec![note(check::NEW_MOUNT, "/mnt/x (ext4) from /dev/sdb1")],
            found: None,
        },
        Call {
            notes: vec![note(
                check::WATCH_DIR_CHANGED,
                "/etc differs from the baseline",
            )],
            found: None,
        },
    ];
    let (mut h, _) = noting("note-exclusive", calls);
    let granted = HeartbeatReply {
        exclusive: Some("whole-1".to_owned()),
        ..reply(vec![])
    };
    h.life.apply(&granted);
    let mut events = Vec::new();
    assert_eq!(h.life.prepare("whole-1", &mut events), Quiet::Ready);
    assert_eq!(names(&events), ["finding - new-mount"]);
    assert_eq!(
        finding_of(&events, check::NEW_MOUNT).1,
        "/mnt/x (ext4) from /dev/sdb1"
    );

    let mut events = Vec::new();
    assert_eq!(h.life.restore(&mut events), Restored::Done);
    assert_eq!(names(&events), ["finding - watch-dir-changed"]);
    assert_eq!(
        finding_of(&events, check::WATCH_DIR_CHANGED).1,
        "/etc differs from the baseline"
    );
    assert!(!h.life.is_tainted());
}

/// Asserts that a re-check of a taint logs what its check notes as findings of no run, at
/// every re-check that sees one, and that a note does not fail the re-check: the two re-checks
/// that note a module still clear the taint. It catches a re-check that swallows the notes and
/// one that counts a note as a finding.
#[test]
fn a_note_at_a_taint_recheck_is_logged_and_does_not_fail_it() {
    let calls = vec![
        Call {
            notes: vec![],
            found: Some(residue()),
        },
        Call {
            notes: vec![note(check::MODULE_CHANGED, "loaded zram")],
            found: None,
        },
        Call {
            notes: vec![note(check::USER_CHANGED, "added builder")],
            found: None,
        },
    ];
    let (mut h, _) = noting("note-recheck", calls);
    h.start(job("r1"));
    h.fake.exit("r1", 0);
    h.life.step();
    ack(&mut h);
    h.life.step();
    assert_eq!(
        names(&tick(&mut h, EVERY)),
        ["finding - kernel-module-changed"]
    );
    assert_eq!(h.life.pending_taint_clear(), None);
    let events = tick(&mut h, EVERY);
    assert_eq!(names(&events), ["finding - user-changed"]);
    assert_eq!(finding_of(&events, check::USER_CHANGED).1, "added builder");
    assert!(h.life.pending_taint_clear().is_some());
    assert_eq!(names(&ack_clear(&mut h)), ["finding - taint-cleared"]);
    assert!(!h.life.is_tainted());
}

/// Asserts that the host keeps the detail of its first taint, one line that says what changed:
/// the finding's detail for a residue, the words "the check could not run" before it for a
/// check that could not, none for a host job, and the first taint's when a second one is
/// sealed beside it; that the detail is gone when the taint clears and for a taint read
/// from the marker at start; and that the event of the new taint carries it. It catches a
/// taint that forgets what changed, a second taint that takes the detail of the first,
/// and a detail that outlives its taint.
#[test]
fn the_first_taint_keeps_what_changed_until_it_clears() {
    let found = CheckFinding::unchecked(check::PROCESSES_UNREADABLE, "no /proc");
    let mut h = Harness::with(
        "detail-unchecked",
        Options {
            findings: vec![("r1", found)],
            ..Options::default()
        },
    );
    h.start(job("r1"));
    h.fake.exit("r1", 0);
    let events = h.life.step();
    let said = Some("the check could not run: no /proc".to_owned());
    assert_eq!(tainted_detail(&events), said);
    assert_eq!(h.life.taint_detail(), said);
    ack(&mut h);
    h.life.step();
    tick(&mut h, EVERY);
    tick(&mut h, EVERY);
    assert_eq!(names(&ack_clear(&mut h)), ["finding - taint-cleared"]);
    assert_eq!(h.life.taint_detail(), None, "gone with the taint");

    let mut h = Harness::with(
        "detail-second",
        Options {
            findings: vec![("r1", residue())],
            ..Options::default()
        },
    );
    h.start(job("r1"));
    h.start(job("r2"));
    h.fake.exit("r1", 0);
    h.life.step();
    h.fake.fail("remove", 1);
    h.fake.exit("r2", 0);
    h.life.step();
    assert_eq!(h.life.taint_detail(), Some("pid 41".to_owned()));
    h.restart().unwrap();
    assert!(h.life.is_tainted());
    assert_eq!(h.life.taint_detail(), None, "the marker holds no detail");

    let mut h = Harness::with(
        "detail-host-job",
        Options {
            allow_host_jobs: true,
            ..Options::default()
        },
    );
    let mut spec = job("r1");
    spec.isolation = Isolation::Host;
    let events = h.start(spec);
    assert_eq!(names(&events)[0], "tainted r1 host-job");
    assert_eq!(tainted_detail(&events), None);
    assert_eq!(h.life.taint_detail(), None);
}

/// Asserts that a taint of each check an older agent tainted for (an orphan, a mount, a user, a
/// group, a kernel module, a watched directory) made in this process is re-checked and clears
/// after two clean re-checks and the server's acknowledgement, the marker kept as a cleared
/// copy and the blocked run freed: what the checks now only note must not hold a host tainted
/// for good. It catches a legacy code that is not clearable.
#[test]
fn an_old_taint_of_a_check_that_no_longer_taints_clears() {
    for what in [
        check::DAEMONISED_CHILD,
        check::NEW_MOUNT,
        check::USER_CHANGED,
        check::GROUP_CHANGED,
        check::MODULE_CHANGED,
        check::WATCH_DIR_CHANGED,
    ] {
        let found = CheckFinding::residue(what, "x");
        let mut h = Harness::with(
            "old-clears",
            Options {
                findings: vec![("r1", found)],
                ..Options::default()
            },
        );
        h.start(job("r1"));
        h.fake.exit("r1", 0);
        h.life.step();
        assert!(h.life.is_tainted(), "{what}");
        ack(&mut h);
        h.life.step();
        tick(&mut h, EVERY);
        assert_eq!(h.life.pending_taint_clear(), None, "{what}");
        tick(&mut h, EVERY);
        assert!(h.life.pending_taint_clear().is_some(), "{what}");
        assert_eq!(
            names(&ack_clear(&mut h)),
            ["finding - taint-cleared"],
            "{what}"
        );
        assert!(!h.life.is_tainted(), "{what}");
        assert_eq!(h.life.free().cpu_millis, 4000, "{what}");
    }
}

/// Asserts that a marker an older agent left for `kernel-module-changed` or `group-changed`,
/// read when the agent starts, is reported, is one a restarted agent re-checks (the update
/// hook's rule, `StateRoot::restored_rechecks`, agrees), keeps no detail, and clears after
/// two clean re-checks and the server's acknowledgement, after which the next start runs. It
/// catches the stale stored taint that stayed pinned until an operator moved the marker.
#[test]
fn a_stored_taint_of_a_check_that_no_longer_taints_clears_after_the_rechecks() {
    for what in [check::MODULE_CHANGED, check::GROUP_CHANGED] {
        let mut h = Harness::new("stored-old");
        let stored = Taint {
            reason: reason::RESIDUE.to_owned(),
            run_id: Some("r1".to_owned()),
            check: Some(what.to_owned()),
        };
        h.config.state.write(&stored).unwrap();
        assert!(h.config.state.restored_rechecks(&stored), "{what}");
        h.restart().unwrap();
        assert_eq!(h.life.pending_taint(), Some(stored), "{what}");
        assert_eq!(h.life.taint_detail(), None, "{what}");
        assert_eq!(names(&h.start(job("r2"))), ["refused r2 host-tainted"]);
        ack(&mut h);
        h.life.step();
        tick(&mut h, EVERY);
        tick(&mut h, EVERY);
        assert!(h.life.pending_taint_clear().is_some(), "{what}");
        assert_eq!(
            names(&ack_clear(&mut h)),
            ["finding - taint-cleared"],
            "{what}"
        );
        assert!(!h.life.is_tainted(), "{what}");
        assert_eq!(names(&h.start(job("r3"))), ["started r3"], "{what}");
    }
}

/// Asserts that a note keeps its code and at most `CHECK_DETAIL_MAX` bytes of detail, cut at a
/// character boundary, so that what a host calls a module or a mount cannot fill the log; a
/// detail that fits is kept whole. It catches a note that is not bounded and a cut that splits
/// a character.
#[test]
fn a_note_keeps_its_code_and_a_bounded_detail() {
    let exact = "a".repeat(CHECK_DETAIL_MAX);
    assert_eq!(
        CheckNote::new(check::NEW_MOUNT, &exact),
        CheckNote {
            code: check::NEW_MOUNT,
            detail: exact,
        }
    );
    let long = "a".repeat(CHECK_DETAIL_MAX + 1);
    assert_eq!(
        CheckNote::new(check::NEW_MOUNT, &long).detail,
        "a".repeat(CHECK_DETAIL_MAX)
    );
    let straddle = format!("{}é", "a".repeat(CHECK_DETAIL_MAX - 1));
    assert_eq!(
        CheckNote::new(check::MODULE_CHANGED, &straddle).detail,
        "a".repeat(CHECK_DETAIL_MAX - 1)
    );
}
