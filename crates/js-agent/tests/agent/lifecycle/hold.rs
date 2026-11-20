//! The hold file: what the lifecycle does while the state root holds the host (every start
//! refused `host-held`, nothing tainted, running work left alone), and the form of the file
//! itself (`hold::read` and `hold::write` over a real state root).

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::PathBuf;

use js_agent::lifecycle::hold::{self, HOLD_FILE, HOLD_TEMP_FILE, HOLD_TEXT, Hold};
use js_agent::lifecycle::{Event, StateRoot};
use js_agent_wire::{Outcome, refusal};

use super::per_job::{job_start, listing, start};
use super::{Harness, job, names};
use crate::support::Scratch;

/// The user the tests run as.
fn me() -> u32 {
    rustix::process::geteuid().as_raw()
}

/// The hold file of the harness's state root.
fn hold_path(h: &Harness) -> PathBuf {
    h.dir.path("state").join(HOLD_FILE)
}

/// The text of the refusal in `events`, which must hold exactly one.
fn why_of(events: &[Event]) -> String {
    let whys: Vec<String> = events
        .iter()
        .filter_map(|event| match event {
            Event::Refused { why, .. } => Some(why.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(whys.len(), 1, "{events:?}");
    whys[0].clone()
}

/// A private state root of its own, opened, under `dir`.
fn root_in(dir: &Scratch, name: &str) -> (StateRoot, PathBuf) {
    let path = dir.path(name);
    fs::create_dir(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    (StateRoot::open(&path).unwrap(), path)
}

/// The permission bits of `path`.
fn mode_of(path: &std::path::Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

/// Asserts that a held host refuses a run start with the code `host-held` and the text of
/// the hold, creates nothing, writes no taint marker, and answers the run
/// `failed_to_start` with the detail `host-held`. It catches a held host that starts the
/// run, one that taints the host for the hold, and one that gives a refusal another code.
#[test]
fn a_held_host_refuses_a_start_without_tainting() {
    let mut h = Harness::new("hold-refuses");
    hold::write(&h.config.state).unwrap();
    let events = h.start(job("r1"));
    assert_eq!(names(&events), ["refused r1 host-held"]);
    assert_eq!(refusal::HOST_HELD, "host-held");
    assert_eq!(why_of(&events), Hold::Held.why().unwrap());
    assert_eq!(
        why_of(&events),
        "this host is held (its hold file is there)"
    );
    assert!(h.fake.created().is_empty());
    assert!(!h.life.is_tainted());
    assert!(!h.marker().exists());
    let results = h.life.results();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].run_id, "r1");
    assert_eq!(results[0].outcome, Outcome::FailedToStart);
    assert_eq!(results[0].detail.as_deref(), Some("host-held"));
}

/// Asserts that a held host refuses a per-job start the same way: the code `host-held`,
/// nothing created, the answer in the job's listing as `failed_to_start` with the detail
/// `host-held`, no run result and no taint. It catches a hold that only covers run starts.
#[test]
fn a_held_host_refuses_a_job_start() {
    let mut h = Harness::new("hold-job");
    hold::write(&h.config.state).unwrap();
    let events = h.apply(vec![job_start(start("j1"))]);
    assert_eq!(names(&events), ["refused j1 host-held"]);
    assert_eq!(why_of(&events), Hold::Held.why().unwrap());
    assert!(h.fake.created().is_empty());
    assert!(h.life.results().is_empty());
    assert!(!h.life.is_tainted());
    let result = listing(&h, "j1").result.expect("a result");
    assert_eq!(result.outcome, Outcome::FailedToStart);
    assert_eq!(result.detail.as_deref(), Some("host-held"));
}

/// Asserts that the hold file is looked at for each start: the start while it is there is
/// refused, and after it is removed the next start runs and creates its container. It
/// catches a hold read once at start-up, and a hold that outlives its file.
#[test]
fn removing_the_hold_file_lets_the_next_start_run() {
    let mut h = Harness::new("hold-removed");
    hold::write(&h.config.state).unwrap();
    assert_eq!(names(&h.start(job("r1"))), ["refused r1 host-held"]);
    fs::remove_file(hold_path(&h)).unwrap();
    assert_eq!(names(&h.start(job("r2"))), ["started r2"]);
    assert_eq!(h.fake.created().len(), 1);
    assert!(!h.life.is_tainted());
}

/// Asserts that a hold written while a run is running refuses the next start and leaves the
/// running run alone: nothing is stopped, the container stays, and the run ends and reports
/// as `succeeded` when it exits. It catches a hold that stops or kills running work.
#[test]
fn a_hold_written_during_a_run_leaves_it_running() {
    let mut h = Harness::new("hold-while-running");
    assert_eq!(names(&h.start(job("r1"))), ["started r1"]);
    hold::write(&h.config.state).unwrap();
    assert_eq!(names(&h.start(job("r2"))), ["refused r2 host-held"]);
    assert!(h.life.step().is_empty(), "r1 is still running");
    assert_eq!(h.fake.created().len(), 1);
    h.fake.exit("r1", 0);
    let events = names(&h.life.step());
    assert_eq!(events[0], "result r1");
    assert!(
        h.life
            .results()
            .iter()
            .any(|r| r.run_id == "r1" && r.outcome == Outcome::Succeeded)
    );
}

/// Asserts that the hold survives a restart of the agent: the new lifecycle refuses the
/// next start, and the file is still there with its text (the agent never removes it). It
/// catches a hold kept in memory, and an agent that clears the hold file at start-up.
#[test]
fn the_hold_survives_a_restart() {
    let mut h = Harness::new("hold-restart");
    hold::write(&h.config.state).unwrap();
    h.restart().unwrap();
    assert_eq!(names(&h.start(job("r1"))), ["refused r1 host-held"]);
    assert_eq!(fs::read_to_string(hold_path(&h)).unwrap(), HOLD_TEXT);
    assert!(h.fake.created().is_empty());
}

/// Asserts that a host whose hold file was never written starts its runs. It catches a
/// hold that is the default.
#[test]
fn a_host_never_held_starts_its_runs() {
    let mut h = Harness::new("hold-never");
    assert!(!hold_path(&h).exists());
    assert_eq!(names(&h.start(job("r1"))), ["started r1"]);
    assert_eq!(h.fake.created().len(), 1);
}

/// Asserts that a hold file that is refused (mode 0644) still holds the host: the start is
/// refused `host-held` with the text that names the mode, and nothing is created. It catches
/// a damaged hold file read as no hold.
#[test]
fn a_refused_hold_file_still_refuses_starts() {
    let mut h = Harness::new("hold-refused-file");
    fs::write(hold_path(&h), HOLD_TEXT).unwrap();
    fs::set_permissions(hold_path(&h), fs::Permissions::from_mode(0o644)).unwrap();
    let events = h.start(job("r1"));
    assert_eq!(names(&events), ["refused r1 host-held"]);
    assert_eq!(
        why_of(&events),
        "this host is held: its hold file is refused (group or others have permission on it \
         (mode 0644)) and holds it until it is removed"
    );
    assert!(h.fake.created().is_empty());
    assert!(!h.life.is_tainted());
}

/// Asserts that a state root with no hold file reads `Absent` and one written by `write`
/// reads `Held`. It catches a reader that finds a hold where there is none, and a writer
/// whose file the reader does not accept.
#[test]
fn read_finds_what_write_wrote() {
    let dir = Scratch::new("hold-file-roundtrip");
    let (root, _) = root_in(&dir, "state");
    assert_eq!(hold::read(&root, me()), Hold::Absent);
    assert_eq!(hold::read_own(&root), Hold::Absent);
    hold::write(&root).unwrap();
    assert_eq!(hold::read(&root, me()), Hold::Held);
    assert_eq!(hold::read_own(&root), Hold::Held);
    hold::write(&root).unwrap();
    assert_eq!(hold::read_own(&root), Hold::Held, "written again");
}

/// Asserts that `write` gives the hold file mode 0600 and the text `HOLD_TEXT`, leaves no
/// temporary file, and sets mode 0600 again on a temporary file a crash left with mode 0644
/// and other text. It catches a hold file others can read and a stale temporary file's mode
/// or text carried over.
#[test]
fn write_is_private_and_replaces_a_stale_temporary_file() {
    let dir = Scratch::new("hold-file-write");
    let (root, path) = root_in(&dir, "state");
    let temp = path.join(HOLD_TEMP_FILE);
    fs::write(
        &temp,
        "left by a crash, longer than the hold text itself ........",
    )
    .unwrap();
    fs::set_permissions(&temp, fs::Permissions::from_mode(0o644)).unwrap();
    hold::write(&root).unwrap();
    let file = root.path().join(HOLD_FILE);
    assert_eq!(fs::read_to_string(&file).unwrap(), HOLD_TEXT);
    assert_eq!(mode_of(&file), 0o600);
    assert!(!temp.exists(), "no temporary file is left");
    let listed: Vec<String> = fs::read_dir(&path)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(listed, [HOLD_FILE]);
}

/// Asserts that a hold file of another user is refused with the text naming both users, and
/// that the host is held all the same. It catches a hold file of someone else taken as the
/// agent's own.
#[test]
fn a_hold_file_of_another_user_is_refused() {
    let dir = Scratch::new("hold-file-owner");
    let (root, path) = root_in(&dir, "state");
    hold::write(&root).unwrap();
    let owner = fs::metadata(path.join(HOLD_FILE)).unwrap().uid();
    let other = me() + 1;
    let read = hold::read(&root, other);
    assert_eq!(
        read,
        Hold::Refused(format!(
            "it is owned by user {owner}, not by the user running the agent ({other})"
        ))
    );
    assert!(read.holds());
}

/// Asserts that a hold file group or others have permission on is refused with the text
/// naming its mode. It catches a hold file anyone could write taken as the agent's.
#[test]
fn a_hold_file_others_can_reach_is_refused() {
    let dir = Scratch::new("hold-file-mode");
    let (root, path) = root_in(&dir, "state");
    hold::write(&root).unwrap();
    fs::set_permissions(path.join(HOLD_FILE), fs::Permissions::from_mode(0o640)).unwrap();
    assert_eq!(
        hold::read_own(&root),
        Hold::Refused("group or others have permission on it (mode 0640)".to_owned())
    );
}

/// Asserts that a link named `hold` (pointing at a valid file of mode 0600) and a directory
/// named `hold` are each refused as not a regular file. It catches a link followed, and a
/// directory taken as a hold file.
#[test]
fn a_link_or_a_directory_is_not_a_hold_file() {
    let dir = Scratch::new("hold-file-kind");
    let text = "it is not a regular file (a link is not accepted)".to_owned();
    let (root, path) = root_in(&dir, "linked");
    let target = dir.write("target", HOLD_TEXT, 0o600);
    symlink(target, path.join(HOLD_FILE)).unwrap();
    assert_eq!(hold::read_own(&root), Hold::Refused(text.clone()));

    let (root, path) = root_in(&dir, "dir");
    fs::create_dir(path.join(HOLD_FILE)).unwrap();
    assert_eq!(hold::read_own(&root), Hold::Refused(text));
}

/// Asserts that a state root that cannot be looked into (its directory replaced by a regular
/// file after it was opened) gives a refusal whose text starts "it cannot be looked at: ",
/// and that the host is still held. It catches a failed look taken for an absent hold.
#[test]
fn a_hold_that_cannot_be_looked_at_is_refused() {
    let dir = Scratch::new("hold-file-unreadable");
    let (root, path) = root_in(&dir, "state");
    fs::remove_dir(&path).unwrap();
    fs::write(&path, "not a directory").unwrap();
    let read = hold::read_own(&root);
    match &read {
        Hold::Refused(why) => assert!(why.starts_with("it cannot be looked at: "), "{why}"),
        other => panic!("not refused: {other:?}"),
    }
    assert!(read.holds());
}

/// Asserts that `holds` is true for a held and a refused hold and false for an absent one,
/// and that `why` gives no text for an absent hold and the exact texts for the others. It
/// catches a refused hold that does not hold, and a refusal text that drifts.
#[test]
fn holds_and_why_of_each_state() {
    assert!(!Hold::Absent.holds());
    assert!(Hold::Held.holds());
    assert!(Hold::Refused("x".to_owned()).holds());
    assert_eq!(Hold::Absent.why(), None);
    assert_eq!(
        Hold::Held.why().as_deref(),
        Some("this host is held (its hold file is there)")
    );
    assert_eq!(
        Hold::Refused("x".to_owned()).why().as_deref(),
        Some("this host is held: its hold file is refused (x) and holds it until it is removed")
    );
}

/// Asserts that `write` into a state root it cannot write (the temporary file's name is a
/// directory; then the state directory replaced by a file) returns an error and leaves no
/// hold. It catches a write that reports success without a hold file.
#[test]
fn a_write_that_fails_leaves_no_hold() {
    let dir = Scratch::new("hold-file-fails");
    let (root, path) = root_in(&dir, "state");
    fs::create_dir(path.join(HOLD_TEMP_FILE)).unwrap();
    assert!(hold::write(&root).is_err());
    assert_eq!(hold::read_own(&root), Hold::Absent);

    let (root, path) = root_in(&dir, "gone");
    fs::remove_dir(&path).unwrap();
    fs::write(&path, "not a directory").unwrap();
    assert!(hold::write(&root).is_err());
}
