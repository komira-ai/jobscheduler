//! A taint that clears itself (`docs/design/taint-self-clearing.md`): the re-checks of an
//! idle host, the clear reported to the server, and what never clears. The runtime, the
//! checks and the clock are the in-memory ones; the marker is a real file in the state root.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::fs;
use std::rc::Rc;
use std::time::Duration;

use js_agent::integrity::code as check;
use js_agent::lifecycle::taint::{CLEARED_PREFIX, reason};
use js_agent::lifecycle::{
    CheckFinding, CheckHook, Event, Finished, MAX_TAINT_RECHECK_MS, clearable, code, recheck_every,
};
use js_agent_wire::Taint;
use serde_json::json;

use super::{Harness, Options, job, names, reply_json};

/// The wait between re-checks when `--taint-recheck-ms` is not given, in milliseconds.
const EVERY: u64 = 60_000;

/// An agent whose check finds `found` after `r1`, with `r1` run and ended: the host is
/// tainted, the run is `Blocked` and its slot quarantined, and no run is held.
fn tainted_by(name: &str, found: CheckFinding) -> Harness {
    let mut h = Harness::with(
        name,
        Options {
            findings: vec![("r1", found)],
            ..Options::default()
        },
    );
    h.start(job("r1"));
    h.fake.exit("r1", 0);
    h.life.step();
    assert!(h.life.is_tainted());
    h
}

fn residue() -> CheckFinding {
    CheckFinding::residue(check::DAEMONISED_CHILD, "pid 41")
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

/// Moves the clock by `ms` and steps.
fn tick(h: &mut Harness, ms: u64) {
    h.clock.advance(ms);
    h.life.step();
}

/// How many re-checks have run.
fn rechecks(h: &Harness) -> usize {
    let log = h.log.borrow();
    log.iter()
        .filter(|line| line.starts_with("check taint-recheck"))
        .count()
}

/// The cleared copies of the marker in the state root.
fn cleared_copies(h: &Harness) -> Vec<String> {
    let names = fs::read_dir(h.dir.path("state")).unwrap();
    let mut found: Vec<String> = names
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(CLEARED_PREFIX))
        .collect();
    found.sort();
    found
}

/// Steps through `intervals` waits of `EVERY` and asserts nothing was re-checked and the
/// host stays tainted, with no clear reported.
fn stays_tainted(h: &mut Harness, intervals: u32) {
    for _ in 0..intervals {
        tick(h, EVERY);
    }
    assert_eq!(rechecks(h), 0);
    assert!(h.life.is_tainted());
    assert_eq!(h.life.pending_taint_clear(), None);
    assert!(h.marker().exists());
}

/// Asserts the whole flow of a clear on an idle host: the first re-check is one wait after
/// the agent first sees the taint, the second one wait after the first, and nothing runs
/// between; after the second the taint is no longer reported but its clear is, and the host
/// is tainted still (a start is refused) until the server acknowledges the clear; on that
/// acknowledgement the marker is renamed to `taint.json.cleared-<time>`, the clear is a
/// finding that names the taint, the blocked run and its slot are freed, and a start is
/// accepted. A reply with `taint_ack` before the second re-check clears nothing. It catches
/// a taint that never clears, a clear after one check, a clear that does not wait for the
/// server, a marker that is deleted instead of kept, and a slot left quarantined.
#[test]
fn a_residue_taint_on_an_idle_host_clears_after_two_clean_rechecks() {
    let mut h = tainted_by("clears", residue());
    assert_eq!(h.life.quarantined_slots(), [0]);
    assert_eq!(
        h.life.free().cpu_millis,
        3000,
        "the blocked run holds its slot"
    );
    let held = h.life.pending_taint().unwrap();
    assert_eq!(ack(&mut h), []);
    assert_eq!(h.life.pending_taint(), None, "the report is acknowledged");

    h.life.step();
    tick(&mut h, EVERY - 1);
    assert_eq!(rechecks(&h), 0, "no re-check before the wait is over");
    tick(&mut h, 1);
    assert_eq!(rechecks(&h), 1);
    assert_eq!(
        h.life.pending_taint_clear(),
        None,
        "one check does not clear"
    );
    assert_eq!(ack(&mut h), [], "an acknowledgement clears nothing");
    assert_eq!(
        ack_clear(&mut h),
        [],
        "nor does one of a clear never reported"
    );
    assert!(h.life.is_tainted());
    tick(&mut h, EVERY - 1);
    assert_eq!(rechecks(&h), 1, "the second one is a wait after the first");
    tick(&mut h, 1);
    assert_eq!(rechecks(&h), 2);

    assert_eq!(h.life.pending_taint_clear(), Some(held.clone()));
    assert_eq!(
        h.life.pending_taint(),
        None,
        "the taint is not reported beside its clear"
    );
    assert!(h.life.is_tainted());
    assert!(h.marker().exists());
    assert_eq!(names(&h.start(job("r2"))), ["refused r2 host-tainted"]);
    tick(&mut h, 3 * EVERY);
    assert_eq!(rechecks(&h), 2, "no more checks while the clear waits");
    assert_eq!(
        ack(&mut h),
        [],
        "the late acknowledgement of the taint report is not the server's word on the clear"
    );
    assert!(h.life.is_tainted());
    assert!(h.life.pending_taint_clear().is_some());

    let events = ack_clear(&mut h);
    assert_eq!(names(&events), ["finding - taint-cleared"]);
    let Event::Finding(finding) = &events[0] else {
        panic!("{events:?}");
    };
    assert!(
        finding
            .message
            .contains("the re-checks passed twice, 60000 ms apart")
            && finding
                .message
                .contains("reason residue, check daemonised-child, run r1")
            && finding.message.contains("taint.json.cleared-"),
        "{}",
        finding.message
    );
    assert!(!h.life.is_tainted());
    assert_eq!(h.life.pending_taint_clear(), None);
    assert!(!h.marker().exists());
    let kept = cleared_copies(&h);
    assert_eq!(kept.len(), 1, "{kept:?}");
    let text = fs::read_to_string(h.dir.path("state").join(&kept[0])).unwrap();
    assert_eq!(serde_json::from_str::<Taint>(&text).unwrap(), held);
    assert_eq!(h.life.quarantined_slots(), Vec::<u32>::new());
    assert_eq!(h.life.free().cpu_millis, 4000, "the blocked run is freed");
    assert_eq!(names(&h.start(job("r3"))), ["started r3"]);
}

/// Asserts that a taint of an input the checks could not read clears the same way. It
/// catches a rule that clears only residue.
#[test]
fn an_unchecked_taint_clears_too() {
    let found = CheckFinding::unchecked(check::PROCESSES_UNREADABLE, "no /proc");
    let mut h = tainted_by("unchecked-clears", found);
    ack(&mut h);
    for _ in 0..3 {
        tick(&mut h, EVERY);
    }
    assert_eq!(rechecks(&h), 2);
    let events = ack_clear(&mut h);
    assert_eq!(names(&events), ["finding - taint-cleared"]);
    assert!(!h.life.is_tainted());
}

/// Asserts that the wait between re-checks is the one the lifecycle was given
/// (`--taint-recheck-ms`). It catches a wait that is the default whatever the flag says.
#[test]
fn the_wait_is_the_one_the_lifecycle_was_given() {
    let mut h = Harness::with(
        "short-wait",
        Options {
            findings: vec![("r1", residue())],
            ..Options::default()
        },
    );
    h.life = h.life.with_taint_recheck(Duration::from_millis(500));
    h.start(job("r1"));
    h.fake.exit("r1", 0);
    h.life.step();
    tick(&mut h, 499);
    assert_eq!(rechecks(&h), 0);
    tick(&mut h, 1);
    tick(&mut h, 500);
    assert_eq!(rechecks(&h), 2);
    assert!(h.life.pending_taint_clear().is_some());
}

/// A check that fails `r1`'s job and answers each re-check from a script: `true` passes,
/// `false` finds the stray process of the first finding.
struct Script {
    verdicts: Rc<RefCell<VecDeque<bool>>>,
}

impl CheckHook for Script {
    fn run(&mut self, job: &Finished<'_>) -> Result<(), CheckFinding> {
        let passes = job.run_id != "r1" && self.verdicts.borrow_mut().pop_front().unwrap_or(true);
        match passes {
            true => Ok(()),
            false => Err(residue()),
        }
    }
}

/// Asserts that a re-check that still finds the process keeps the taint, however many wait
/// periods pass, and that the count of clean re-checks starts again after it: a clean one,
/// a finding and a clean one do not clear, and neither do two findings; two clean ones in
/// a row after them do. The finding is not
/// reported as a new taint (the first stays). It catches a count that is not of consecutive
/// checks, and a stray process that is forgotten after a while.
#[test]
fn a_recheck_that_still_finds_the_process_keeps_the_taint() {
    let verdicts = Rc::new(RefCell::new(VecDeque::from([
        true, false, true, false, false,
    ])));
    let mut h = Harness::with(
        "still-there",
        Options {
            hook: Some(Box::new(Script {
                verdicts: verdicts.clone(),
            })),
            ..Options::default()
        },
    );
    h.start(job("r1"));
    h.fake.exit("r1", 0);
    h.life.step();
    assert!(h.life.is_tainted());
    ack(&mut h);
    h.life.step();
    for _ in 0..5 {
        tick(&mut h, EVERY);
        assert_eq!(h.life.pending_taint_clear(), None);
        assert!(h.life.is_tainted());
    }
    assert!(verdicts.borrow().is_empty(), "five re-checks ran");
    tick(&mut h, EVERY);
    assert_eq!(
        h.life.pending_taint_clear(),
        None,
        "one clean check after a finding"
    );
    tick(&mut h, EVERY);
    assert!(h.life.pending_taint_clear().is_some());
    assert_eq!(names(&ack_clear(&mut h)), ["finding - taint-cleared"]);
}

/// Asserts that a run that is held keeps the re-checks off, and that the count starts when
/// it is over: `r2` runs beside `r1`, whose check taints the host; nothing is re-checked for
/// as long as `r2` runs, however long that is, and the first re-check is a wait after `r2`'s
/// cleanup. It catches a re-check that runs beside a job (its processes are not strays).
#[test]
fn nothing_is_rechecked_while_a_run_is_held() {
    let mut h = Harness::with(
        "held",
        Options {
            findings: vec![("r1", residue())],
            ..Options::default()
        },
    );
    let mut long = job("r2");
    long.limits.wall_ms = 3_600_000;
    h.start(job("r1"));
    h.start(long);
    h.fake.exit("r1", 0);
    h.life.step();
    assert!(h.life.is_tainted());
    ack(&mut h);
    for _ in 0..4 {
        tick(&mut h, EVERY);
    }
    assert_eq!(rechecks(&h), 0);

    h.fake.exit("r2", 0);
    h.life.step();
    tick(&mut h, EVERY - 1);
    assert_eq!(rechecks(&h), 0);
    tick(&mut h, 1);
    assert_eq!(rechecks(&h), 1);
}

/// Asserts that a taint the server never acknowledged clears by reporting its clear: the
/// taint is reported until the second clean re-check, and from then on only its clear is
/// (the server that never heard of the taint finds nothing to forget and acknowledges the
/// clear all the same). It catches a clear that reports both, or one that waits for an
/// acknowledgement of the taint.
#[test]
fn a_taint_the_server_never_acknowledged_reports_its_clear_instead() {
    let mut h = tainted_by("unacked", residue());
    h.life.step();
    tick(&mut h, EVERY);
    assert!(h.life.pending_taint().is_some());
    assert_eq!(h.life.pending_taint_clear(), None);
    tick(&mut h, EVERY);
    assert_eq!(h.life.pending_taint(), None);
    assert!(h.life.pending_taint_clear().is_some());
    assert_eq!(names(&ack_clear(&mut h)), ["finding - taint-cleared"]);
    assert!(!h.life.is_tainted());
}

/// Asserts that a taint whose marker could not be written is not re-checked until it is on
/// disk (the server was never told of it, and a clear renames the marker): with the
/// temporary file blocked nothing is re-checked however long the host is idle, and once the
/// marker is written the waits begin. It catches a clear of a taint that a crash would
/// have forgotten anyway, and one that skips the file.
#[test]
fn a_taint_that_is_not_on_disk_is_not_rechecked() {
    let mut h = Harness::with(
        "not-on-disk",
        Options {
            findings: vec![("r1", residue())],
            ..Options::default()
        },
    );
    let blocker = h
        .dir
        .path("state")
        .join(js_agent::lifecycle::taint::MARKER_TEMP_FILE);
    fs::create_dir(&blocker).unwrap();
    h.start(job("r1"));
    h.fake.exit("r1", 0);
    h.life.step();
    assert!(h.life.is_tainted());
    assert!(!h.marker().exists());
    for _ in 0..3 {
        tick(&mut h, EVERY);
    }
    assert_eq!(rechecks(&h), 0);

    fs::remove_dir(&blocker).unwrap();
    h.life.step();
    assert!(h.marker().exists());
    tick(&mut h, EVERY);
    tick(&mut h, EVERY);
    assert_eq!(rechecks(&h), 2);
    assert!(h.life.pending_taint_clear().is_some());
}

/// Asserts that every run the taint holds blocked is freed with it, and the others are left
/// alone: `r0` ends clean (its result is held, no slot), `r1` and `r2` both find a process
/// (the second taint is the first one's kind, so the host still clears), and after the clear
/// both slots are free again, no owed record is left, and four runs fit the host. It catches
/// a clear that frees one blocked run, one that touches a run that was never blocked, and
/// a slot left quarantined.
#[test]
fn every_blocked_run_is_freed_when_the_taint_clears() {
    let mut h = Harness::with(
        "many-blocked",
        Options {
            findings: vec![("r1", residue()), ("r2", residue())],
            ..Options::default()
        },
    );
    h.start(job("r0"));
    h.fake.exit("r0", 0);
    h.life.step();
    h.start(job("r1"));
    h.start(job("r2"));
    h.fake.exit("r1", 0);
    h.fake.exit("r2", 0);
    h.life.step();
    assert_eq!(h.life.quarantined_slots().len(), 2);
    ack(&mut h);
    h.life.step();
    tick(&mut h, EVERY);
    tick(&mut h, EVERY);
    assert_eq!(names(&ack_clear(&mut h)), ["finding - taint-cleared"]);
    assert_eq!(h.life.quarantined_slots(), Vec::<u32>::new());
    assert_eq!(h.life.free().cpu_millis, 4000);
    assert!(!h.dir.path("state").join("owed-r1").exists());
    assert!(!h.dir.path("state").join("owed-r2").exists());
    for run in ["r3", "r4", "r5", "r6"] {
        assert_eq!(names(&h.start(job(run))), [format!("started {run}")]);
    }
}

/// Asserts that taints a re-check cannot speak for never clear, however long the host is
/// idle: a failed container removal (`cleanup-failed`), a mount, a user, a group, a module
/// and a watched directory that changed (each a residue of a check that is not about a
/// process), a check owed at start, and a taint of a run that may change the host. It
/// catches a rule that clears by reason alone, and one that treats a quiet host as proof
/// that a change was undone.
#[test]
fn what_the_host_really_holds_never_clears() {
    for found in [
        check::NEW_MOUNT,
        check::USER_CHANGED,
        check::GROUP_CHANGED,
        check::MODULE_CHANGED,
        check::WATCH_DIR_CHANGED,
    ] {
        let mut h = tainted_by("changed", CheckFinding::residue(found, "x"));
        ack(&mut h);
        stays_tainted(&mut h, 4);
    }

    let mut h = Harness::new("remove-failed");
    h.fake.fail("remove", 1);
    h.start(job("r1"));
    h.fake.exit("r1", 0);
    h.life.step();
    assert_eq!(
        h.life.pending_taint().unwrap().reason,
        reason::CLEANUP_FAILED
    );
    h.life.step();
    ack(&mut h);
    stays_tainted(&mut h, 4);

    let mut h = Harness::new("owed");
    h.start(job("r1"));
    h.fake.exit("r1", 0);
    h.fake.fail("remove", u32::MAX);
    h.life.step();
    fs::remove_file(h.marker()).unwrap();
    h.restart().unwrap();
    assert_eq!(h.life.pending_taint().unwrap().reason, reason::CHECK_OWED);
    h.fake.fail("remove", 0);
    h.life.step();
    ack(&mut h);
    stays_tainted(&mut h, 4);

    let mut h = Harness::with(
        "host-job",
        Options {
            allow_host_jobs: true,
            ..Options::default()
        },
    );
    let mut spec = job("r1");
    spec.isolation = js_agent_wire::Isolation::Host;
    h.start(spec);
    h.fake.exit("r1", 0);
    h.life.step();
    assert_eq!(h.life.pending_taint().unwrap().reason, reason::HOST_JOB);
    ack(&mut h);
    stays_tainted(&mut h, 4);
}

/// Asserts that a taint that cannot be cleared, sealed beside one that can, keeps the host
/// tainted: `r1`'s check finds a process (the first taint, the one in the marker) and `r2`'s
/// container cannot be removed (`cleanup-failed`, not recorded, the first stays); once
/// `r2` is gone the host is idle and the process is gone, and the host still does not
/// clear. It catches the first taint standing for everything the host holds.
#[test]
fn a_taint_that_cannot_clear_sealed_after_one_that_can_pins_the_host() {
    let mut h = Harness::with(
        "pinned",
        Options {
            findings: vec![("r1", residue())],
            ..Options::default()
        },
    );
    h.start(job("r1"));
    h.start(job("r2"));
    h.fake.exit("r1", 0);
    h.life.step();
    ack(&mut h);
    h.fake.exit("r2", 0);
    h.fake.fail("remove", 1);
    h.life.step();
    h.life.step();
    assert_eq!(
        h.life.findings().len(),
        2,
        "the removal failed and was noted"
    );
    assert_eq!(h.life.pending_taint(), None);
    stays_tainted(&mut h, 4);
}

/// Asserts that an agent that restarts after it proposed a clear and before the server
/// acknowledged it starts over: the marker is still there, the host is tainted, the taint is
/// reported again and not its clear. It catches a clear that is final before the server
/// agrees, which would leave the server tainted for good.
#[test]
fn a_restart_before_the_acknowledgement_starts_over() {
    let mut h = tainted_by("restart", residue());
    ack(&mut h);
    h.life.step();
    tick(&mut h, EVERY);
    tick(&mut h, EVERY);
    assert!(h.life.pending_taint_clear().is_some());
    h.restart().unwrap();
    assert!(h.life.is_tainted());
    assert_eq!(h.life.pending_taint_clear(), None);
    assert_eq!(h.life.pending_taint().unwrap().reason, reason::RESIDUE);
    assert!(h.marker().exists());
    assert_eq!(cleared_copies(&h), Vec::<String>::new());
}

/// Asserts that a taint restored from the marker never clears, even with the process still
/// present at every re-check: the restarted agent's baseline holds that process, so its
/// re-checks pass; no re-check runs, the host stays tainted and nothing is reported cleared.
/// It catches a restart that lets two clean re-checks clear a taint whose residue is still
/// there.
#[test]
fn a_restored_taint_never_clears_with_its_process_still_present() {
    let mut h = tainted_by("restored", residue());
    ack(&mut h);
    h.restart().unwrap();
    assert!(h.life.is_tainted());
    stays_tainted(&mut h, 5);
    assert_eq!(cleared_copies(&h), Vec::<String>::new());
}

/// Asserts that a clear that cannot be finished leaves the host tainted and says so: the
/// record that the blocked run's checks were owed cannot be removed (a directory stands in
/// its place), the acknowledgement yields a `taint-clear-failed` finding, the clear is still
/// reported, nothing is renamed and the slot stays quarantined; once the record can go, the
/// next acknowledgement finishes it. It catches a clear that frees the host half way.
#[test]
fn a_clear_that_cannot_finish_leaves_the_host_tainted_and_is_tried_again() {
    let mut h = tainted_by("clear-fails", residue());
    ack(&mut h);
    h.life.step();
    tick(&mut h, EVERY);
    tick(&mut h, EVERY);
    assert!(h.life.pending_taint_clear().is_some());
    let record = h.dir.path("state").join("owed-r1");
    fs::create_dir(&record).unwrap();
    fs::write(record.join("held"), "x").unwrap();

    let events = ack_clear(&mut h);
    assert_eq!(names(&events), ["finding - taint-clear-failed"]);
    assert!(h.life.is_tainted());
    assert!(h.life.pending_taint_clear().is_some());
    assert!(h.marker().exists());
    assert_eq!(h.life.quarantined_slots(), [0]);

    fs::remove_dir_all(&record).unwrap();
    assert_eq!(names(&ack_clear(&mut h)), ["finding - taint-cleared"]);
    assert!(!h.life.is_tainted());
}

/// Asserts that a clear whose marker was moved away by an operator in the meantime still
/// finishes: the file that is not there is already kept. It catches a clear that fails for
/// good on an operator's own clear.
#[test]
fn a_marker_an_operator_moved_is_already_kept() {
    let mut h = tainted_by("moved", residue());
    ack(&mut h);
    h.life.step();
    tick(&mut h, EVERY);
    tick(&mut h, EVERY);
    fs::rename(h.marker(), h.dir.path("state").join("taint.json.cleared-1")).unwrap();
    assert_eq!(names(&ack_clear(&mut h)), ["finding - taint-cleared"]);
    assert!(!h.life.is_tainted());
    assert_eq!(cleared_copies(&h), ["taint.json.cleared-1"]);
}

/// Asserts which taints [`clearable`] says the agent may clear: the residue of a process and
/// the unchecked of an input that could not be read, and nothing else, a taint with no check
/// or an unknown one included. It catches a code added to the checks and cleared by default.
#[test]
fn only_a_process_or_an_unread_input_may_clear() {
    let taint = |why: &str, what: Option<&str>| Taint {
        reason: why.to_owned(),
        run_id: Some("r1".to_owned()),
        check: what.map(str::to_owned),
    };
    for what in [check::PROCESS_AS_JOB_USER, check::DAEMONISED_CHILD] {
        assert!(clearable(&taint(reason::RESIDUE, Some(what))), "{what}");
    }
    for what in [
        check::PROCESSES_UNREADABLE,
        check::MOUNTS_UNREADABLE,
        check::USERS_UNREADABLE,
        check::GROUPS_UNREADABLE,
        check::MODULES_UNREADABLE,
        check::WATCH_UNREADABLE,
        check::WATCH_OVER_BOUND,
    ] {
        assert!(clearable(&taint(reason::UNCHECKED, Some(what))), "{what}");
    }
    for refused in [
        taint(reason::RESIDUE, Some(check::NEW_MOUNT)),
        taint(reason::RESIDUE, Some(check::USER_CHANGED)),
        taint(reason::RESIDUE, Some(check::GROUP_CHANGED)),
        taint(reason::RESIDUE, Some(check::MODULE_CHANGED)),
        taint(reason::RESIDUE, Some(check::WATCH_DIR_CHANGED)),
        taint(reason::RESIDUE, Some(check::PROCESSES_UNREADABLE)),
        taint(reason::RESIDUE, None),
        taint(reason::UNCHECKED, Some(code::HOST_NOT_VISIBLE)),
        taint(reason::UNCHECKED, Some(check::DAEMONISED_CHILD)),
        taint(reason::UNCHECKED, None),
        taint(reason::CLEANUP_FAILED, Some(check::DAEMONISED_CHILD)),
        taint(reason::CHECK_OWED, None),
        taint(reason::HOST_JOB, None),
        taint(reason::MARKER_UNREADABLE, None),
        taint("something-new", Some(check::DAEMONISED_CHILD)),
    ] {
        assert!(!clearable(&refused), "{refused:?}");
    }
}

/// Asserts the bounds of `--taint-recheck-ms`: a minute when not given, 1 and the maximum
/// pass, 0 and one above are refused naming the flag. It catches a flag with no bounds.
#[test]
fn the_wait_between_rechecks_is_bounded() {
    assert_eq!(recheck_every(None), Ok(Duration::from_secs(60)));
    assert_eq!(recheck_every(Some(1)), Ok(Duration::from_millis(1)));
    assert_eq!(
        recheck_every(Some(MAX_TAINT_RECHECK_MS)),
        Ok(Duration::from_millis(MAX_TAINT_RECHECK_MS))
    );
    assert_eq!(
        recheck_every(Some(0)),
        Err("--taint-recheck-ms: at least 1".to_owned())
    );
    assert_eq!(
        recheck_every(Some(MAX_TAINT_RECHECK_MS + 1)),
        Err("--taint-recheck-ms: at most 3600000".to_owned())
    );
}

/// Asserts that a marker that cannot be kept under its cleared name is an error and stays
/// the marker (a directory stands where the copy would go), and that keeping a marker that
/// is not there is not one. It catches a clear that loses the marker when the rename fails.
#[test]
fn a_marker_that_cannot_be_renamed_stays() {
    let h = Harness::new("keep-fails");
    let state = &h.config.state;
    state
        .write(&Taint {
            reason: reason::RESIDUE.to_owned(),
            run_id: None,
            check: None,
        })
        .unwrap();
    let blocker = h.dir.path("state").join("taint.json.cleared-7");
    fs::create_dir(&blocker).unwrap();
    fs::write(blocker.join("held"), "x").unwrap();
    assert!(state.keep_cleared(7).is_err());
    assert_eq!(state.read().unwrap().reason, reason::RESIDUE);

    assert!(state.keep_cleared(8).is_ok());
    assert!(state.read().is_none());
    assert!(state.keep_cleared(9).is_ok(), "nothing to keep");
}
