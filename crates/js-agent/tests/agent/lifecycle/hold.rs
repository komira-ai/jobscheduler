//! The hold file: what the lifecycle does while the state root holds the host (every start
//! refused `host-held`, nothing tainted, running work left alone), and the form of the file
//! itself (`hold::read` and `hold::write` over a real state root).

use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::PathBuf;

use js_agent::lifecycle::hold::{
    self, HOLD_FILE, HOLD_RESUME_FILE, HOLD_TEMP_FILE, HOLD_TEXT, Hold,
};
use js_agent::lifecycle::{Event, StateRoot};
use js_agent::update_hook::{HoldState, Holds, PortError};
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
    let results = super::results(&h.life);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].run_id, "r1");
    assert_eq!(results[0].outcome, Outcome::FailedToStart);
    assert_eq!(results[0].detail.as_deref(), Some("host-held"));
}

/// Asserts that a held host refuses a per-job start the same way: the code `host-held`,
/// nothing created, the answer in the job's listing as `failed_to_start` with the detail
/// `host-held`, and no taint. It catches a hold that a job start does not
/// meet.
#[test]
fn a_held_host_refuses_a_job_start() {
    let mut h = Harness::new("hold-job");
    hold::write(&h.config.state).unwrap();
    let events = h.apply(vec![job_start(start("j1"))]);
    assert_eq!(names(&events), ["refused j1 host-held"]);
    assert_eq!(why_of(&events), Hold::Held.why().unwrap());
    assert!(h.fake.created().is_empty());
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
        super::results(&h.life)
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

/// The text of a hold file with a note, for the tests of the text forms.
const NOTED: &str = "a note of two lines\ncordoned_before=true\n";

/// Asserts that `write_text` writes exactly the text it is given (not `HOLD_TEXT`), with mode
/// 0600, leaves no temporary file, and replaces a longer text of an earlier hold whole. It
/// catches a note that is appended to the old text or cut short, and a hold file others can
/// read.
#[test]
fn write_text_writes_the_text_exactly_with_mode_0600() {
    let dir = Scratch::new("hold-text-write");
    let (root, path) = root_in(&dir, "state");
    let file = path.join(HOLD_FILE);
    hold::write_text(&root, NOTED).unwrap();
    assert_eq!(fs::read_to_string(&file).unwrap(), NOTED);
    assert_eq!(mode_of(&file), 0o600);
    assert_eq!(hold::read_own(&root), Hold::Held);
    hold::write_text(&root, "short\n").unwrap();
    assert_eq!(fs::read_to_string(&file).unwrap(), "short\n");
    assert_eq!(mode_of(&file), 0o600);
    let listed: Vec<String> = fs::read_dir(&path)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(listed, [HOLD_FILE]);
}

/// Asserts that `read_text` answers `None` for a file that is not there, and the file's whole
/// text for a regular one, by either name (the hold file, or the one `release` renamed it to).
/// It catches an absent file read as an error, and a text that is cut or changed.
#[test]
fn read_text_finds_none_where_nothing_is_and_the_text_where_a_file_is() {
    let dir = Scratch::new("hold-text-read");
    let (root, path) = root_in(&dir, "state");
    assert_eq!(hold::read_text(&root, HOLD_FILE).unwrap(), None);
    assert_eq!(hold::read_text(&root, HOLD_RESUME_FILE).unwrap(), None);
    hold::write_text(&root, NOTED).unwrap();
    assert_eq!(
        hold::read_text(&root, HOLD_FILE).unwrap().as_deref(),
        Some(NOTED)
    );
    assert_eq!(hold::read_text(&root, HOLD_RESUME_FILE).unwrap(), None);
    fs::write(path.join(HOLD_RESUME_FILE), "renamed\n").unwrap();
    assert_eq!(
        hold::read_text(&root, HOLD_RESUME_FILE).unwrap().as_deref(),
        Some("renamed\n")
    );
}

/// Asserts that `read_text` refuses a directory and a link under the name it is asked for
/// with `InvalidData` and the exact text naming that name, a link even when it points at a
/// good file; that bytes that are not text are `InvalidData` too; and that a state root that
/// cannot be looked into (its directory replaced by a file) is an error that is not
/// "not found". It catches a link followed, a directory read as absent, and a failed look
/// taken for no hold.
#[test]
fn read_text_refuses_what_is_not_a_regular_file_and_what_cannot_be_looked_at() {
    let dir = Scratch::new("hold-text-refused");
    let (root, path) = root_in(&dir, "dir");
    fs::create_dir(path.join(HOLD_FILE)).unwrap();
    let err = hold::read_text(&root, HOLD_FILE).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidData);
    assert_eq!(
        err.to_string(),
        "hold is not a regular file (a link is not accepted)"
    );

    let (root, path) = root_in(&dir, "linked");
    let target = dir.write("target", HOLD_TEXT, 0o600);
    symlink(target, path.join(HOLD_RESUME_FILE)).unwrap();
    let err = hold::read_text(&root, HOLD_RESUME_FILE).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidData);
    assert_eq!(
        err.to_string(),
        "hold.resume is not a regular file (a link is not accepted)"
    );

    let (root, path) = root_in(&dir, "bytes");
    fs::write(path.join(HOLD_FILE), [0xff, 0xfe, 0x00]).unwrap();
    let err = hold::read_text(&root, HOLD_FILE).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidData);

    let (root, path) = root_in(&dir, "gone");
    fs::remove_dir(&path).unwrap();
    fs::write(&path, "not a directory").unwrap();
    let err = hold::read_text(&root, HOLD_FILE).unwrap_err();
    assert_ne!(err.kind(), ErrorKind::NotFound);
}

/// Asserts that `release` renames the hold file to `hold.resume` with its text: the hold file
/// is gone, the agent's own reading of the hold is `Absent` (it stops holding the host), and
/// the renamed file is the text of the hold; and that with no hold file it fails `NotFound`
/// and makes no `hold.resume`. It catches a release that copies, one that leaves the agent
/// holding the host, and one that succeeds with nothing to release.
#[test]
fn release_renames_the_hold_file_and_fails_without_one() {
    let dir = Scratch::new("hold-text-release");
    let (root, path) = root_in(&dir, "state");
    hold::write_text(&root, NOTED).unwrap();
    assert_eq!(hold::read_own(&root), Hold::Held);
    hold::release(&root).unwrap();
    assert_eq!(hold::read_own(&root), Hold::Absent);
    assert!(!path.join(HOLD_FILE).exists());
    assert_eq!(
        fs::read_to_string(path.join(HOLD_RESUME_FILE)).unwrap(),
        NOTED
    );
    assert_eq!(mode_of(&path.join(HOLD_RESUME_FILE)), 0o600);

    let (root, path) = root_in(&dir, "empty");
    let err = hold::release(&root).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::NotFound);
    assert!(!path.join(HOLD_RESUME_FILE).exists());
}

/// Asserts that `released` removes `hold.resume` and leaves the hold file alone, is no error
/// when there is none, and fails when it cannot remove it (a non-empty directory is there).
/// It catches a release that removes the hold file, and one that reports success over a file
/// it left.
#[test]
fn released_removes_the_renamed_file_and_fails_when_it_cannot() {
    let dir = Scratch::new("hold-text-released");
    let (root, path) = root_in(&dir, "state");
    hold::write_text(&root, NOTED).unwrap();
    fs::write(path.join(HOLD_RESUME_FILE), "renamed\n").unwrap();
    hold::released(&root).unwrap();
    assert!(!path.join(HOLD_RESUME_FILE).exists());
    assert_eq!(fs::read_to_string(path.join(HOLD_FILE)).unwrap(), NOTED);
    hold::released(&root).unwrap();

    let (root, path) = root_in(&dir, "blocked");
    let blocker = path.join(HOLD_RESUME_FILE);
    fs::create_dir(&blocker).unwrap();
    fs::write(blocker.join("inner"), "x").unwrap();
    let err = hold::released(&root).unwrap_err();
    assert_ne!(err.kind(), ErrorKind::NotFound);
    assert!(blocker.join("inner").exists());
}

/// Asserts what the state root says of the hold to the steps: `Free` with neither file,
/// `Held(text)` with the hold file (whatever `hold.resume` holds beside it), and
/// `Releasing(text)` with only the renamed file; and that `hold` writes the text, `release`
/// renames it and `released` removes the renamed file, through the trait. It catches a state
/// that reads the renamed file before the hold file, and a trait that does not reach the file.
#[test]
fn the_state_root_tells_the_steps_free_held_and_releasing() {
    let dir = Scratch::new("hold-port-states");
    let (root, path) = root_in(&dir, "state");
    assert_eq!(root.state().unwrap(), HoldState::Free);
    root.hold(NOTED).unwrap();
    assert_eq!(root.state().unwrap(), HoldState::Held(NOTED.to_owned()));
    assert_eq!(fs::read_to_string(path.join(HOLD_FILE)).unwrap(), NOTED);
    fs::write(path.join(HOLD_RESUME_FILE), "older\n").unwrap();
    assert_eq!(root.state().unwrap(), HoldState::Held(NOTED.to_owned()));
    fs::remove_file(path.join(HOLD_RESUME_FILE)).unwrap();
    root.release().unwrap();
    assert!(!path.join(HOLD_FILE).exists());
    assert_eq!(
        root.state().unwrap(),
        HoldState::Releasing(NOTED.to_owned())
    );
    root.released().unwrap();
    assert_eq!(root.state().unwrap(), HoldState::Free);
    assert!(fs::read_dir(&path).unwrap().next().is_none());
}

/// Asserts that every call of the state root's `Holds` that fails is a `PortError::Io`: a
/// directory under either name (with the exact text), a release with no hold file, a removal
/// of a renamed file that cannot be removed, and a write into a state root whose directory
/// became a file. It catches an error of the file system told as a command's or lost.
#[test]
fn the_errors_of_the_state_roots_holds_are_io_errors() {
    let dir = Scratch::new("hold-port-errors");
    let (root, path) = root_in(&dir, "state");
    fs::create_dir(path.join(HOLD_FILE)).unwrap();
    assert_eq!(
        root.state(),
        Err(PortError::Io(
            "hold is not a regular file (a link is not accepted)".to_owned()
        ))
    );
    fs::remove_dir(path.join(HOLD_FILE)).unwrap();
    fs::create_dir(path.join(HOLD_RESUME_FILE)).unwrap();
    assert_eq!(
        root.state(),
        Err(PortError::Io(
            "hold.resume is not a regular file (a link is not accepted)".to_owned()
        ))
    );
    fs::write(path.join(HOLD_RESUME_FILE).join("inner"), "x").unwrap();
    assert!(matches!(root.released(), Err(PortError::Io(_))));
    assert!(matches!(root.release(), Err(PortError::Io(_))));

    let (root, path) = root_in(&dir, "gone");
    fs::remove_dir(&path).unwrap();
    fs::write(&path, "not a directory").unwrap();
    assert!(matches!(root.hold(NOTED), Err(PortError::Io(_))));
    assert!(matches!(root.state(), Err(PortError::Io(_))));
}
