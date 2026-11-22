//! A taint that clears itself (`docs/design/taint-self-clearing.md`): the re-checks of an
//! idle host, the clear reported to the server, and what never clears. The runtime, the
//! checks and the clock are the in-memory ones; the marker is a real file in the state root.

use std::cell::{Cell, RefCell};
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
    CheckFinding::residue(check::PROCESS_AS_JOB_USER, "pid 41")
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
                .contains("reason residue, check process-as-job-user, run r1")
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

/// Asserts that a crun run whose container group was not found a settle wait after the first
/// sighting, while its first process ran in another group, taints the host as unchecked with
/// `cgroup-not-found`, and that the taint clears
/// after two clean re-checks of the idle host and the server's acknowledgement of the clear.
/// Catches a group that was slow to appear leaving a node out of service until an operator
/// moves the marker aside.
#[test]
fn a_cgroup_not_found_taint_clears_after_two_clean_rechecks() {
    let mut h = Harness::new("cgroup-not-found-clears");
    h.fake.promise_cgroups(true);
    h.fake.run_elsewhere("r1", "/actions/other");
    h.start(job("r1"));
    h.life.step();
    h.clock.advance(1000);
    h.life.step();
    let held = Taint {
        reason: reason::UNCHECKED.to_owned(),
        run_id: Some("r1".to_owned()),
        check: Some(code::CGROUP_NOT_FOUND.to_owned()),
    };
    assert_eq!(h.life.pending_taint(), Some(held.clone()));
    h.fake.exit("r1", 0);
    h.life.step();
    ack(&mut h);
    for _ in 0..3 {
        tick(&mut h, EVERY);
    }
    assert_eq!(rechecks(&h), 2);
    assert_eq!(h.life.pending_taint_clear(), Some(held));
    let events = ack_clear(&mut h);
    assert_eq!(names(&events), ["finding - taint-cleared"]);
    assert!(!h.life.is_tainted());
    assert!(!h.marker().exists());
}

/// Asserts that a start whose container the agent's process table cannot see (the engine
/// path, `host-not-visible`) is never re-checked and keeps the host tainted however long it
/// idles, unlike `cgroup-not-found`. Catches the two codes being treated alike, so that a
/// host the agent cannot see is cleared by checks that cannot see it.
#[test]
fn a_host_not_visible_taint_of_a_start_never_clears() {
    let mut h = Harness::new("host-not-visible-stays");
    h.fake.blind_next_create();
    h.start(job("r1"));
    assert!(h.life.is_tainted());
    ack(&mut h);
    stays_tainted(&mut h, 4);
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
/// idle: a failed container removal (`cleanup-failed`), a check owed at start, a taint of a
/// run that may change the host, and a residue that names no check. It catches a rule that
/// clears by reason alone, and one that treats a quiet host as proof that a change was
/// undone.
#[test]
fn what_the_host_really_holds_never_clears() {
    let mut h = tainted_by("no-check", CheckFinding::residue("unheard-of", "x"));
    ack(&mut h);
    stays_tainted(&mut h, 4);

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

/// The marker of a taint of `why` with the check `what`, written into `h`'s state root as
/// an agent that stopped with it would have left it.
fn leave_marker(h: &Harness, why: &str, what: Option<&str>) {
    let taint = Taint {
        reason: why.to_owned(),
        run_id: Some("r1".to_owned()),
        check: what.map(str::to_owned),
    };
    h.config.state.write(&taint).unwrap();
}

/// A check that passes `run` (the re-check against the baseline) and answers
/// `run_restored` (the re-check of a taint read from the marker) from `present`: the process
/// is there while it is set. It fails `r1`'s job with a residue, and notes which of the two
/// re-checks ran.
struct Restored {
    present: Rc<Cell<bool>>,
    calls: Rc<RefCell<Vec<&'static str>>>,
}

impl CheckHook for Restored {
    fn run(&mut self, job: &Finished<'_>) -> Result<(), CheckFinding> {
        if job.run_id == "r1" {
            return Err(residue());
        }
        self.calls.borrow_mut().push("run");
        Ok(())
    }

    fn run_restored(&mut self, _job: &Finished<'_>) -> Result<(), CheckFinding> {
        self.calls.borrow_mut().push("restored");
        match self.present.get() {
            true => Err(residue()),
            false => Ok(()),
        }
    }
}

/// Asserts that a residue of a post-job check read from the marker at start clears like a fresh one:
/// the taint is reported again after the restart, two clean re-checks a wait apart report
/// the clear, and the server's acknowledgement ends the taint, keeps the marker as
/// `taint.json.cleared-<time>` and lets the next start run. It holds for the check of a
/// process and for each check an older agent tainted for what the checks now only note (an
/// orphan, a mount, a user, a group, a kernel module, a watched directory): the marker such
/// an agent left clears at the first clean re-checks. Catches a restart that leaves the host
/// tainted until an operator moves the marker, which took every build node out of service for
/// a process that was long gone, and a stored taint of a demoted check that is never re-checked.
#[test]
fn a_restored_residue_taint_clears_after_two_clean_rechecks() {
    for what in [
        check::PROCESS_AS_JOB_USER,
        check::DAEMONISED_CHILD,
        check::NEW_MOUNT,
        check::USER_CHANGED,
        check::GROUP_CHANGED,
        check::MODULE_CHANGED,
        check::WATCH_DIR_CHANGED,
    ] {
        let mut h = tainted_by("restored-clears", CheckFinding::residue(what, "pid 41"));
        ack(&mut h);
        h.restart().unwrap();
        assert!(h.life.is_tainted());
        assert_eq!(h.life.pending_taint().unwrap().reason, reason::RESIDUE);
        ack(&mut h);
        h.life.step();
        tick(&mut h, EVERY - 1);
        assert_eq!(rechecks(&h), 0, "{what}: no re-check before the wait");
        tick(&mut h, 1);
        assert_eq!(
            h.life.pending_taint_clear(),
            None,
            "{what}: one clean check"
        );
        tick(&mut h, EVERY);
        assert_eq!(rechecks(&h), 2, "{what}");
        assert!(h.life.pending_taint_clear().is_some(), "{what}");
        assert_eq!(names(&ack_clear(&mut h)), ["finding - taint-cleared"]);
        assert!(!h.life.is_tainted(), "{what}");
        assert_eq!(cleared_copies(&h).len(), 1, "{what}");
        assert!(!h.marker().exists(), "{what}");
        assert_eq!(names(&h.start(job("r2"))), ["started r2"], "{what}");
    }
}

/// Asserts that the re-check of a restored taint is the one that needs no baseline, and that
/// it keeps the host tainted for as long as it finds the process: five waits pass with the
/// process there and nothing clears, each of them ran `run_restored`; when the process is
/// gone two re-checks clear the taint. Catches a restored taint that is re-checked against
/// the restart baseline alone, which holds the process and passes.
#[test]
fn a_restored_recheck_that_still_finds_the_process_keeps_the_taint() {
    let present = Rc::new(Cell::new(true));
    let calls = Rc::new(RefCell::new(Vec::new()));
    let mut h = tainted_by("restored-present", residue());
    ack(&mut h);
    h.restart_with(Box::new(Restored {
        present: present.clone(),
        calls: calls.clone(),
    }))
    .unwrap();
    ack(&mut h);
    h.life.step();
    for _ in 0..5 {
        tick(&mut h, EVERY);
        assert_eq!(h.life.pending_taint_clear(), None);
        assert!(h.life.is_tainted());
    }
    assert_eq!(*calls.borrow(), ["restored"; 5]);

    present.set(false);
    tick(&mut h, EVERY);
    assert_eq!(h.life.pending_taint_clear(), None, "one clean check");
    tick(&mut h, EVERY);
    assert!(h.life.pending_taint_clear().is_some());
    assert_eq!(names(&ack_clear(&mut h)), ["finding - taint-cleared"]);
}

/// Asserts that a taint made in this process is re-checked with `run`, the check against the
/// baseline, and never with the restored one: the baseline was taken before the job, so the
/// baseline-free rules would add findings it does not need. Catches a lifecycle that gives
/// every taint the restored re-check.
#[test]
fn a_taint_made_in_this_process_is_rechecked_against_the_baseline() {
    let calls = Rc::new(RefCell::new(Vec::new()));
    let mut h = Harness::with(
        "fresh-baseline",
        Options {
            hook: Some(Box::new(Restored {
                present: Rc::new(Cell::new(true)),
                calls: calls.clone(),
            })),
            ..Options::default()
        },
    );
    h.start(job("r1"));
    h.fake.exit("r1", 0);
    h.life.step();
    ack(&mut h);
    h.life.step();
    tick(&mut h, EVERY);
    tick(&mut h, EVERY);
    assert_eq!(*calls.borrow(), ["run", "run"]);
    assert!(h.life.pending_taint_clear().is_some());
}

/// Asserts that a restored taint that is not a residue of a post-job check stays until an operator
/// moves the marker aside, with the host idle for any number of waits: an input that could
/// not be read, `host-not-visible`, a failed removal, a check owed, a host job, an unreadable
/// marker, and a residue that names no check or one that reads an input. Catches a restart
/// that lets a re-check of the restart baseline clear a taint it cannot speak for.
#[test]
fn a_restored_taint_that_is_not_a_check_residue_stays_pinned() {
    for (why, what) in [
        (reason::UNCHECKED, Some(check::PROCESSES_UNREADABLE)),
        (reason::UNCHECKED, Some(check::MOUNTS_UNREADABLE)),
        (reason::UNCHECKED, Some(code::HOST_NOT_VISIBLE)),
        (reason::CLEANUP_FAILED, Some(code::CONTAINER_REMOVE_FAILED)),
        (reason::CHECK_OWED, None),
        (reason::ACCOUNT_NOT_DELETED, Some(code::ACCOUNT_HELD)),
        (reason::HOST_JOB, None),
        (reason::RESIDUE, None),
        (reason::RESIDUE, Some(check::PROCESSES_UNREADABLE)),
    ] {
        let mut h = Harness::new("restored-pinned");
        leave_marker(&h, why, what);
        h.restart().unwrap();
        assert!(h.life.is_tainted(), "{why} {what:?}");
        ack(&mut h);
        stays_tainted(&mut h, 4);
    }

    let mut h = Harness::new("restored-unreadable");
    fs::write(h.marker(), "not json").unwrap();
    h.restart().unwrap();
    assert_eq!(
        h.life.pending_taint().unwrap().reason,
        reason::MARKER_UNREADABLE
    );
    ack(&mut h);
    stays_tainted(&mut h, 4);
}

/// Asserts that a restored residue taint beside the records of owed checks stays pinned: the
/// agent that stopped left the marker and the record of a run whose checks never ran, the new
/// process taints the host `check-owed` beside it (the marker keeps the first), and no
/// number of clean re-checks clears it. Catches a restored taint that clears while the host
/// also holds a taint no re-check can speak for.
#[test]
fn a_restored_residue_taint_with_owed_records_stays_pinned() {
    let mut h = Harness::new("restored-owed");
    leave_marker(&h, reason::RESIDUE, Some(check::DAEMONISED_CHILD));
    h.config.state.owe("r9").unwrap();
    h.restart().unwrap();
    assert_eq!(h.life.pending_taint().unwrap().reason, reason::RESIDUE);
    ack(&mut h);
    stays_tainted(&mut h, 4);
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

/// Asserts which taints [`clearable`] says the agent may clear: the residue of a post-job check
/// (the process of a job's user, and the six codes an older agent tainted for what the checks
/// now only note) and the unchecked of an input that could not be read or of a run whose
/// container group was not found, and nothing else (`host-not-visible` of an engine runtime
/// included), a taint with no check or an unknown one included. It catches a code added to the
/// checks and cleared by default, a legacy code left off the allow-list (its marker would stay
/// pinned), and `cgroup-not-found` left off the allow-list.
#[test]
fn only_a_post_job_check_or_an_unread_input_may_clear() {
    let taint = |why: &str, what: Option<&str>| Taint {
        reason: why.to_owned(),
        run_id: Some("r1".to_owned()),
        check: what.map(str::to_owned),
    };
    for what in [
        check::PROCESS_AS_JOB_USER,
        check::DAEMONISED_CHILD,
        check::NEW_MOUNT,
        check::USER_CHANGED,
        check::GROUP_CHANGED,
        check::MODULE_CHANGED,
        check::WATCH_DIR_CHANGED,
    ] {
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
        code::CGROUP_NOT_FOUND,
        js_agent::leak_scan::code::SCAN_UNREADABLE,
        js_agent::leak_scan::code::SCAN_FAILED,
    ] {
        assert!(clearable(&taint(reason::UNCHECKED, Some(what))), "{what}");
    }
    for refused in [
        taint(reason::RESIDUE, Some(check::PROCESSES_UNREADABLE)),
        taint(reason::UNCHECKED, Some(check::NEW_MOUNT)),
        taint(reason::UNCHECKED, Some(check::USER_CHANGED)),
        taint(reason::HOST_JOB, Some(check::GROUP_CHANGED)),
        taint(reason::CLEANUP_FAILED, Some(check::MODULE_CHANGED)),
        taint(reason::RESIDUE, None),
        taint(reason::UNCHECKED, Some(code::HOST_NOT_VISIBLE)),
        taint(reason::UNCHECKED, Some(check::DAEMONISED_CHILD)),
        taint(
            reason::RESIDUE,
            Some(js_agent::leak_scan::code::SCAN_RESIDUE),
        ),
        taint(
            reason::RESIDUE,
            Some(js_agent::leak_scan::code::SCAN_UNREADABLE),
        ),
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
