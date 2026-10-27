//! The checks that a job owes: a record in the state root from before the first step of the
//! job's cleanup until its checks have run, so that an agent that stops in between does not
//! forget that the host was never checked.

use std::cell::{Cell, RefCell};
use std::fs;
use std::path::PathBuf;
use std::rc::Rc;

use js_agent::lifecycle::taint::reason;
use js_agent::lifecycle::{CheckFinding, CheckHook, Finished};

use super::{Harness, Options, cleanup, job, names};

/// The record of the checks owed by `run_id`.
fn owed(h: &Harness, run_id: &str) -> PathBuf {
    h.dir.path("state").join(format!("owed-{run_id}"))
}

/// Blocks the temporary file of the taint marker, so that no marker can be written.
fn block_marker(h: &Harness) {
    fs::create_dir(h.dir.path("state").join("taint.json.tmp")).unwrap();
}

/// Asserts when the record exists: absent while the job runs, present when its container is
/// removed and when its checks run (the fake runtime and the checks note whether the
/// watched file exists), and gone once the checks passed and the slot is free. It catches a
/// record written after the container is gone (a crash between would lose the debt), and one
/// that is never taken back.
#[test]
fn the_checks_are_owed_from_before_the_cleanup_until_they_have_run() {
    let mut h = Harness::new("owed-order");
    h.fake.watch_marker(owed(&h, "r1"));
    h.start(job("r1"));
    assert!(!owed(&h, "r1").exists(), "nothing is owed while it runs");
    h.fake.exit("r1", 0);
    let events = names(&h.life.step());
    assert_eq!(
        events,
        [vec!["result r1".to_owned()], cleanup("r1")].concat()
    );
    assert_eq!(
        *h.log.borrow(),
        [
            "create r1 [no marker]",
            "remove c1 (scratch present) [marker present]",
            "check r1 (container gone, scratch gone) [marker present]"
        ]
    );
    assert!(!owed(&h, "r1").exists(), "taken back once the checks ran");
}

/// Asserts that an agent that stopped between the start of a job's cleanup and the end of its
/// checks taints the host when it starts again, naming the run, and still takes the job's
/// container over; and that a state root with no record taints nothing. The cleanup is
/// stopped by a container that cannot be removed and the taint that follows is deleted, as
/// if the agent had died before it wrote it. It catches a restart that absorbs what the job
/// left into its baseline.
#[test]
fn a_check_that_was_owed_when_the_agent_stopped_taints_the_next_start() {
    let mut h = Harness::new("owed-start");
    h.start(job("r1"));
    h.fake.exit("r1", 0);
    h.fake.fail("remove", u32::MAX);
    h.life.step();
    assert!(owed(&h, "r1").exists(), "the cleanup began");
    fs::remove_file(h.marker()).unwrap();

    let events = names(&h.restart().unwrap());
    assert_eq!(events, ["tainted r1 check-owed", "adopted r1"]);
    let taint = h.life.pending_taint().unwrap();
    assert_eq!(
        (taint.reason.as_str(), taint.run_id.as_deref(), taint.check),
        (reason::CHECK_OWED, Some("r1"), None)
    );
    assert!(h.marker().exists(), "and it is on disk");
    assert_eq!(names(&h.start(job("r2"))), ["refused r2 host-tainted"]);

    let mut clean = Harness::new("owed-none");
    clean.start(job("r1"));
    assert!(
        names(&clean.restart().unwrap())
            .iter()
            .all(|e| !e.starts_with("tainted"))
    );
    assert!(!clean.life.is_tainted());
}

/// Asserts that records that cannot be listed when the agent starts taint the host with the
/// code that says so (the state root is gone, so the marker cannot be written either and
/// that is a finding). It catches a start that reads an unreadable state root as no debt.
#[test]
fn owed_records_that_cannot_be_listed_taint_the_next_start() {
    let mut h = Harness::new("owed-unlisted");
    fs::remove_dir_all(h.dir.path("state")).unwrap();
    let events = names(&h.restart().unwrap());
    assert_eq!(
        events,
        ["tainted - check-owed", "finding - taint-write-failed"]
    );
    let taint = h.life.pending_taint();
    assert_eq!(taint, None, "not reported while it is not on disk");
    assert!(h.life.is_tainted());
}

/// Asserts that a job whose checks found something takes the record back once the taint is
/// on disk, and keeps it when the taint could not be written (a crash then still taints the
/// next start). It catches a record taken back before the taint is safe.
#[test]
fn a_finding_takes_the_record_back_only_once_the_taint_is_on_disk() {
    let finding = || vec![("r1", CheckFinding::residue("stray-process", "pid 41"))];
    let mut h = Harness::with(
        "owed-finding",
        Options {
            findings: finding(),
            ..Options::default()
        },
    );
    h.start(job("r1"));
    h.fake.exit("r1", 0);
    h.life.step();
    assert!(h.marker().exists());
    assert!(!owed(&h, "r1").exists());

    let mut h = Harness::with(
        "owed-finding-blocked",
        Options {
            findings: finding(),
            ..Options::default()
        },
    );
    block_marker(&h);
    h.start(job("r1"));
    h.fake.exit("r1", 0);
    h.life.step();
    assert!(!h.marker().exists());
    assert!(owed(&h, "r1").exists(), "the taint is not safe yet");
}

/// Asserts that a record that cannot be written holds the cleanup where it is: the
/// container is not removed (the log has no removal), the job says so as a finding, and
/// when the record can be written the cleanup runs to the end. It catches a cleanup that
/// removes the evidence of a job it cannot promise to check.
#[test]
fn a_record_that_cannot_be_written_holds_the_container() {
    let mut h = Harness::new("owed-unwritable");
    fs::create_dir(owed(&h, "r1")).unwrap();
    h.start(job("r1"));
    h.fake.exit("r1", 0);
    let events = names(&h.life.step());
    assert_eq!(events, ["result r1", "finding r1 owed-record-failed"]);
    assert!(
        h.log
            .borrow()
            .iter()
            .all(|line| !line.starts_with("remove")),
        "{:?}",
        h.log.borrow()
    );
    assert_eq!(h.life.free().cpu_millis, 3000);
    assert!(!h.life.is_tainted(), "nothing was left behind");

    fs::remove_dir(owed(&h, "r1")).unwrap();
    assert_eq!(names(&h.life.step()), cleanup("r1"));
    assert_eq!(h.life.free().cpu_millis, 4000);
}

/// Checks that pass, and the first time they run put a directory where the record of the
/// owed checks is, so that the record cannot be removed.
struct Trap {
    path: Rc<RefCell<PathBuf>>,
    spent: Cell<bool>,
}

impl CheckHook for Trap {
    fn run(&mut self, _job: &Finished<'_>) -> Result<(), CheckFinding> {
        if !self.spent.replace(true) {
            let path = self.path.borrow();
            fs::remove_file(&*path).unwrap();
            fs::create_dir(&*path).unwrap();
        }
        Ok(())
    }
}

/// Asserts that a record that cannot be taken back holds the slot, as a finding, and that
/// the checks run again and free the slot when it can. It catches a job that is called
/// checked while a debt for its checks is still on disk.
#[test]
fn a_record_that_cannot_be_removed_holds_the_slot() {
    let record = Rc::new(RefCell::new(PathBuf::new()));
    let mut h = Harness::with(
        "owed-trap",
        Options {
            hook: Some(Box::new(Trap {
                path: Rc::clone(&record),
                spent: Cell::new(false),
            })),
            ..Options::default()
        },
    );
    let path = owed(&h, "r1");
    record.replace(path.clone());
    h.start(job("r1"));
    h.fake.exit("r1", 0);
    let events = names(&h.life.step());
    assert_eq!(
        events,
        [
            "result r1",
            "container removed r1",
            "scratch removed r1",
            "finding r1 owed-record-failed"
        ]
    );
    assert_eq!(h.life.free().cpu_millis, 3000, "the slot is held");
    fs::remove_dir(&path).unwrap();
    assert_eq!(
        names(&h.life.step()),
        ["checks passed r1", "freed r1"],
        "the checks ran again"
    );
    assert_eq!(h.life.free().cpu_millis, 4000);
}

/// Asserts the state root's records of owed checks on their own: they are listed sorted
/// by run, private to their owner, replaced by a second record of the same run, removed by
/// settling (a record that is not there is settled already), and more of them than the
/// agent could have made are an error and not a long list. It catches a list that drops a
/// record, a record other users can read, and an unbounded read of the directory.
#[test]
fn the_state_root_lists_and_settles_the_records_of_owed_checks() {
    use js_agent::lifecycle::StateRoot;
    use js_agent::lifecycle::taint::OWED_MAX;
    use std::os::unix::fs::PermissionsExt;

    let h = Harness::new("owed-records");
    let state = StateRoot::open(&h.dir.path("state")).unwrap();
    assert_eq!(state.owed().unwrap(), Vec::<String>::new());
    state.owe("b").unwrap();
    state.owe("a").unwrap();
    state.owe("a").unwrap();
    assert_eq!(state.owed().unwrap(), ["a", "b"]);
    let mode = fs::metadata(owed(&h, "a")).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    state.settle("a").unwrap();
    state.settle("a").unwrap();
    assert_eq!(state.owed().unwrap(), ["b"]);
    assert!(
        fs::read_dir(h.dir.path("state"))
            .unwrap()
            .all(|entry| entry.unwrap().file_name() == "owed-b"),
        "nothing else is left"
    );

    for n in 0..=OWED_MAX {
        fs::write(h.dir.path("state").join(format!("owed-n{n}")), "").unwrap();
    }
    let why = state.owed().unwrap_err().to_string();
    assert_eq!(why, "more than 1024 records of owed checks");
}

/// Asserts that a cleanup that fails and is tried again still ends in the checks, and that
/// the slot is freed only after they ran clean: the container is removed on the second
/// step, the checks run after it, and a job whose checks find something stays blocked. It
/// pins the rule that a retried cleanup does not free a slot by itself.
#[test]
fn a_retried_cleanup_still_ends_in_the_checks() {
    let mut h = Harness::new("owed-retry-clean");
    h.start(job("r1"));
    h.fake.fail("remove", 1);
    h.fake.exit("r1", 0);
    h.life.step();
    assert_eq!(h.life.free().cpu_millis, 3000, "not freed by the failure");
    h.log.borrow_mut().clear();
    assert_eq!(names(&h.life.step()), cleanup("r1"));
    assert_eq!(
        *h.log.borrow(),
        [
            "remove c1 (scratch present)",
            "check r1 (container gone, scratch gone)"
        ],
        "the checks ran after the retried removal"
    );

    let mut h = Harness::with(
        "owed-retry-dirty",
        Options {
            findings: vec![("r1", CheckFinding::residue("stray-process", "pid 41"))],
            ..Options::default()
        },
    );
    h.start(job("r1"));
    h.fake.fail("remove", 1);
    h.fake.exit("r1", 0);
    h.life.step();
    let events = names(&h.life.step());
    assert_eq!(events.last().unwrap(), "finding r1 stray-process");
    assert_eq!(
        h.life.free().cpu_millis,
        3000,
        "a dirty retry frees nothing"
    );
}
