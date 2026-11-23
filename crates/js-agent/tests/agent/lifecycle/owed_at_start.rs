//! The checks an earlier process owed: a process of the agent that ends inside a job's cleanup
//! window (an update's hand-over or drain that ends with a cleanup under way, a second stop, a
//! crash) leaves the `owed-*` record of the job, and the next process runs the check itself,
//! by the rules that need no baseline, instead of tainting the host. It still taints when the
//! check cannot run or finds something, when a record is not complete, and when its hook cannot
//! judge without a baseline (`owed.rs`).

use std::cell::RefCell;
use std::fs;
use std::path::PathBuf;
use std::rc::Rc;

use js_agent::lifecycle::taint::reason;
use js_agent::lifecycle::{CheckFinding, CheckHook, CheckNote, Finished, StateRoot, label};

use super::deferring::Deferring;
use super::{Harness, cleanup, job, names};

/// What the hook writes, in order.
type Said = Rc<RefCell<Vec<String>>>;

/// Post-job checks that pass, and whose restored check (the one that needs no baseline) says
/// what it was given and answers as the test scripted it.
struct Judging {
    said: Said,
    /// What the restored check answers; it passes when `None`.
    verdict: Option<CheckFinding>,
    /// A note the restored check makes.
    note: Option<CheckNote>,
    /// A record the restored check turns into a directory, so that it cannot be removed.
    trap: Option<PathBuf>,
}

impl Judging {
    fn new(said: &Said) -> Self {
        Self {
            said: Rc::clone(said),
            verdict: None,
            note: None,
            trap: None,
        }
    }

    fn boxed(self) -> Box<dyn CheckHook> {
        Box::new(self)
    }
}

impl CheckHook for Judging {
    fn run(&mut self, job: &Finished<'_>) -> Result<(), CheckFinding> {
        self.said.borrow_mut().push(format!("check {}", job.run_id));
        Ok(())
    }

    fn run_restored(&mut self, job: &Finished<'_>) -> Result<(), CheckFinding> {
        let live: Vec<&str> = job.live.iter().map(|id| id.0.as_str()).collect();
        self.said.borrow_mut().push(format!(
            "restored {} live [{}] slots {:?} slot {:?}",
            job.run_id,
            live.join(","),
            job.live_slots,
            job.slot
        ));
        if let Some(path) = &self.trap {
            fs::remove_file(path).unwrap();
            fs::create_dir(path).unwrap();
        }
        self.verdict.clone().map_or(Ok(()), Err)
    }

    fn notes(&mut self) -> Vec<CheckNote> {
        self.note.take().into_iter().collect()
    }

    fn checks_owed_at_start(&self) -> bool {
        true
    }
}

/// The state root of `h`.
fn state(h: &Harness) -> StateRoot {
    StateRoot::open(&h.dir.path("state")).unwrap()
}

/// The records of owed checks in the state root of `h`, by file name.
fn records(h: &Harness) -> Vec<String> {
    let names = fs::read_dir(h.dir.path("state")).unwrap();
    let mut names: Vec<String> = names
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("owed-"))
        .collect();
    names.sort();
    names
}

/// The taint the host reports, as (reason, run, check), and its detail.
fn taint(h: &Harness) -> (String, Option<String>, Option<String>, Option<String>) {
    let taint = h.life.pending_taint().expect("a taint on disk");
    (
        taint.reason,
        taint.run_id,
        taint.check,
        h.life.taint_detail(),
    )
}

/// Asserts what an update that ends the agent inside a job's cleanup leaves: the job `r1`
/// ended and its container's removal was under way (its record of owed checks on disk) beside
/// a running job `r2` when the process ended, as a hand-over whose bound passed does. The next
/// process takes both containers over, runs the owed check once with both containers live,
/// removes the record and is not tainted; it then cleans `r1` up itself, its own check passes,
/// no record is left, and a new job starts. Catches a start that taints the host for a check
/// it could run (`check-owed`, the host out of service until an operator clears it), and one
/// that judges a running neighbour's processes as the finished job's residue.
#[test]
fn an_update_during_a_runs_cleanup_leaves_no_owed_record_and_no_taint() {
    let (mut h, gate) = Deferring::harness("owed-start-update");
    h.start(job("r1"));
    h.start(job("r2"));
    gate.hold_removal("c1");
    h.fake.exit("r1", 0);
    h.life.step();
    assert_eq!(records(&h), ["owed-r1"], "the cleanup began");

    let said = Said::default();
    let events = names(&h.restart_with(Judging::new(&said).boxed()).unwrap());
    assert_eq!(
        events,
        ["adopted r1", "adopted r2", "finding - owed-checked"]
    );
    assert_eq!(
        *said.borrow(),
        ["restored r1 live [c1,c2] slots [0, 1] slot None"]
    );
    assert!(records(&h).is_empty(), "the record went with the check");
    assert!(!h.life.is_tainted());
    assert!(!h.marker().exists());

    let events = names(&h.life.step());
    assert_eq!(
        events,
        cleanup("r1"),
        "its result was sent before the update"
    );
    assert_eq!(said.borrow().last().unwrap(), "check r1");
    assert!(
        records(&h).is_empty(),
        "its own cleanup took its record back"
    );
    assert_eq!(names(&h.start(job("r3"))), ["started r3"]);
}

/// Asserts that an agent that starts with complete records of owed checks of runs that left
/// nothing on the host runs the check once, naming the first run, with nothing live, removes
/// every record, logs that it did (the finding names how many runs and the first), and is not
/// tainted; a restart after it finds nothing to check. Catches records that taint a host whose
/// check passes, a check run once for each record, and records kept after a clean check (a
/// later start would check or taint again).
#[test]
fn an_agent_that_starts_with_a_complete_owed_record_runs_the_check_and_clears_it() {
    let mut h = Harness::new("owed-start-clean");
    for run in ["r7", "r5", "r6"] {
        state(&h).owe(run).unwrap();
    }
    let said = Said::default();
    let events = h.restart_with(Judging::new(&said).boxed()).unwrap();
    assert_eq!(names(&events), ["finding - owed-checked"]);
    let js_agent::lifecycle::Event::Finding(finding) = &events[0] else {
        panic!("{events:?}");
    };
    assert_eq!(
        finding.message,
        "the post-job checks owed by 3 run(s) of an earlier process (r5 first) ran at the start \
         and passed; their records are removed"
    );
    assert_eq!(*said.borrow(), ["restored r5 live [] slots [] slot None"]);
    assert!(records(&h).is_empty());
    assert!(!h.life.is_tainted());

    said.borrow_mut().clear();
    let events = names(&h.restart_with(Judging::new(&said).boxed()).unwrap());
    assert!(events.is_empty(), "{events:?}");
    assert!(
        said.borrow().is_empty(),
        "nothing is owed, nothing is checked"
    );
}

/// Asserts that the owed check counts every container on the host as live: one taken over
/// (with its slot) and one that could not be removed, and that what the check notes is logged
/// as a finding of no run, whatever it answered. Catches a check that judges what a container
/// still on the host runs as the finished job's leftover, and notes that are dropped.
#[test]
fn the_owed_check_sees_every_container_on_the_host_live_and_logs_its_notes() {
    let mut h = Harness::new("owed-start-live");
    h.start(job("r2"));
    // Labels a contained run carries, but for a booking that cannot be read.
    let odd = [
        (label::RUN, "odd"),
        (label::LEASE, "lease-old"),
        (label::CPU_MILLIS, "lots"),
        (label::MEMORY_BYTES, "1073741824"),
        (label::DISK_BYTES, "0"),
        (label::GPUS, ""),
        (label::WALL_MS, "60000"),
        (label::ISOLATION, "contained"),
    ];
    let stuck = h.fake.leak(&odd, 0);
    h.fake.fail("remove", 1);
    state(&h).owe("r1").unwrap();
    let said = Said::default();
    let hook = Judging {
        note: Some(CheckNote::new("new-mount", "/mnt/x")),
        ..Judging::new(&said)
    };
    let events = names(&h.restart_with(hook.boxed()).unwrap());
    assert_eq!(
        events,
        [
            "adopted r2",
            "tainted - cleanup-failed",
            "finding - unadoptable-container-kept",
            "finding - new-mount",
            "finding - owed-checked"
        ]
    );
    assert_eq!(
        *said.borrow(),
        [format!("restored r1 live [c1,{stuck}] slots [0] slot None")]
    );
    assert!(records(&h).is_empty());
}

/// Asserts that an owed check that cannot run, or that finds a process, still taints the host
/// `check-owed`, naming the first run and the check's code, with what it found as the detail
/// ("the check could not run" before it for a check that could not), and keeps every record,
/// so that a restart taints the host again; a new job is refused. Catches a start that clears
/// records it could not check, or a residue it found, and a taint that says nothing of why.
#[test]
fn an_owed_check_that_cannot_run_or_finds_a_process_still_taints() {
    for (name, found, detail) in [
        (
            "owed-start-blind",
            CheckFinding::unchecked("processes-unreadable", "the process table: EACCES"),
            "the check could not run: the process table: EACCES",
        ),
        (
            "owed-start-residue",
            CheckFinding::residue("process-as-job-user", "pid 41 uid 100000 (sleep)"),
            "pid 41 uid 100000 (sleep)",
        ),
    ] {
        let mut h = Harness::new(name);
        state(&h).owe("r1").unwrap();
        state(&h).owe("r2").unwrap();
        let said = Said::default();
        let hook = Judging {
            verdict: Some(found.clone()),
            ..Judging::new(&said)
        };
        let events = names(&h.restart_with(hook.boxed()).unwrap());
        assert_eq!(events, ["tainted r1 check-owed"], "{name}");
        assert_eq!(
            taint(&h),
            (
                reason::CHECK_OWED.to_owned(),
                Some("r1".to_owned()),
                Some(found.code().to_owned()),
                Some(detail.to_owned())
            ),
            "{name}"
        );
        assert_eq!(records(&h), ["owed-r1", "owed-r2"], "{name}: kept");
        assert_eq!(names(&h.start(job("r3"))), ["refused r3 host-tainted"]);

        let again = Judging::new(&said);
        h.restart_with(again.boxed()).unwrap();
        assert!(h.life.is_tainted(), "{name}: the marker holds it");
    }
}

/// Asserts that a record that does not hold its run id (left empty by a write cut short,
/// holding another run's id, or holding more than the id) taints the host `check-owed` with
/// `owed-record-incomplete`, naming that record's run, without running the check, and that
/// every record is kept. Catches a check run on records it cannot trust, and an incomplete
/// record read as complete because it starts with the run id.
#[test]
fn an_incomplete_owed_record_taints_without_a_check() {
    for (name, body) in [
        ("owed-start-empty", ""),
        ("owed-start-other", "r1"),
        ("owed-start-longer", "r2x"),
    ] {
        let mut h = Harness::new(name);
        state(&h).owe("r1").unwrap();
        fs::write(h.dir.path("state").join("owed-r2"), body).unwrap();
        let said = Said::default();
        let events = names(&h.restart_with(Judging::new(&said).boxed()).unwrap());
        assert_eq!(events, ["tainted r2 check-owed"], "{name}");
        let taint = h.life.pending_taint().unwrap();
        assert_eq!(
            (taint.run_id.as_deref(), taint.check.as_deref()),
            (Some("r2"), Some("owed-record-incomplete")),
            "{name}"
        );
        assert!(said.borrow().is_empty(), "{name}: no check ran");
        assert_eq!(records(&h), ["owed-r1", "owed-r2"], "{name}");
    }
}

/// Asserts that a clean owed check whose record cannot be removed taints the host
/// `check-owed` with `owed-record-failed`, naming the first run and why, since the record
/// stays and would be found again. Catches a host called clean beside a record a restart reads.
#[test]
fn an_owed_record_that_cannot_be_removed_after_a_clean_check_taints() {
    let mut h = Harness::new("owed-start-trap");
    state(&h).owe("r1").unwrap();
    let said = Said::default();
    let hook = Judging {
        trap: Some(h.dir.path("state").join("owed-r1")),
        ..Judging::new(&said)
    };
    let events = names(&h.restart_with(hook.boxed()).unwrap());
    assert_eq!(events, ["tainted r1 check-owed"]);
    let (reason, run, check, detail) = taint(&h);
    assert_eq!(
        (reason.as_str(), run.as_deref(), check.as_deref()),
        (reason::CHECK_OWED, Some("r1"), Some("owed-record-failed"))
    );
    let detail = detail.unwrap();
    assert!(
        detail.starts_with("a record of owed checks cannot be removed: "),
        "{detail}"
    );
    assert_eq!(records(&h), ["owed-r1"]);
}

/// Asserts the state root's test of a complete record on its own: a record [`StateRoot::owe`]
/// wrote is complete, and one that is not there, is a directory, is empty, or holds anything
/// but its run id is not. Catches a test that reads a missing or unreadable record as
/// complete.
#[test]
fn the_state_root_says_which_records_of_owed_checks_are_complete() {
    let h = Harness::new("owed-complete");
    let root = state(&h);
    root.owe("r1").unwrap();
    assert!(root.owed_complete("r1"));
    assert!(!root.owed_complete("r9"), "not there");
    fs::create_dir(h.dir.path("state").join("owed-d")).unwrap();
    assert!(!root.owed_complete("d"), "a directory");
    for body in ["", "r", "r1", "r22", "r2\n"] {
        fs::write(h.dir.path("state").join("owed-r2"), body).unwrap();
        assert!(!root.owed_complete("r2"), "{body:?}");
    }
}
