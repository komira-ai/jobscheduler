//! The kept copy is reached through the state directory the step opened and checked, not by the
//! directory's path: a state directory moved away and replaced by a link once the step has
//! opened it gets the copy's writes, reads, renames and removals, and the link's target gets
//! none of them. The steps run as root, so a path followed again after the check is a way to
//! make them write, rename or remove somewhere else.

use std::fs;
use std::io;
use std::os::fd::BorrowedFd;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use js_agent::update_hook::config::{
    Done, Env, Params, Record, cleanup, cutover, rollback, verify,
};
use js_agent::update_hook::{DirStat, Outcome};

use super::super::step_rig::as_root;
use super::rig::{Rig, Say};

/// The flags the host ran on before the update.
const BEFORE: &str = "--server=https://before.example:7300\n";

/// The flags the package places.
const AFTER: &str = "--server=https://after.example:7300\n";

/// What a file the test puts where the swapped path leads holds.
const DECOY: &str = "not the hook's\n";

/// A state directory that is swapped for a link to `elsewhere` once the step has opened it: the
/// `stat` the step looks at it through (which shows every directory as root's 0700, as the rig's
/// does) moves the directory to `moved` and puts the link at its path when it is shown the
/// directory itself, which the walk has opened by then.
#[derive(Clone)]
struct Swap {
    /// The state directory's real location, as the walk names it.
    state: PathBuf,
    /// Where the opened directory is moved to.
    moved: PathBuf,
    /// Where the link at the state directory's path leads.
    elsewhere: PathBuf,
}

impl Swap {
    fn new(rig: &Rig) -> Swap {
        fs::create_dir_all(&rig.params.state_dir).unwrap();
        let elsewhere = rig.root.join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        Swap {
            state: fs::canonicalize(&rig.params.state_dir).unwrap(),
            moved: rig.root.join("moved"),
            elsewhere,
        }
    }

    fn stat(&self, path: &Path, fd: BorrowedFd<'_>) -> io::Result<DirStat> {
        if path == self.state {
            fs::rename(&self.state, &self.moved).unwrap();
            symlink(&self.elsewhere, &self.state).unwrap();
        }
        as_root(path, fd)
    }

    /// The directory put back at its path, the link removed.
    fn undo(&self) {
        fs::remove_file(&self.state).unwrap();
        fs::rename(&self.moved, &self.state).unwrap();
    }

    /// Runs `step` with the directory swapped after the open, then puts it back at its path.
    fn run(&self, rig: &Rig, step: impl FnOnce(&Env<'_>, &Params) -> Done) -> Done {
        let swap = self.clone();
        let done = rig.run_with(&move |path, fd| swap.stat(path, fd), step);
        self.undo();
        done
    }

    /// [`Swap::run`] for `prepare`.
    fn prepare(&self, rig: &Rig) -> Done {
        let swap = self.clone();
        let done = rig.prepare_with(&move |path, fd| swap.stat(path, fd));
        self.undo();
        done
    }

    /// The names in the link's target, sorted.
    fn elsewhere(&self) -> Vec<String> {
        names(&self.elsewhere)
    }
}

/// The names in `dir`, sorted.
fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Writes `text` at `path` with `mode`, its directories made.
fn write(path: &Path, text: &str, mode: u32) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

/// The installer's activate for a package of the flags file alone.
fn install(rig: &Rig) {
    let temp = rig.root.join(".installing");
    fs::write(&temp, AFTER).unwrap();
    fs::rename(&temp, &rig.params.flags_file).unwrap();
}

/// The kept copy's directory.
fn kept(rig: &Rig) -> PathBuf {
    rig.params.state_dir.join("kept")
}

/// The flags file's text.
fn flags_text(rig: &Rig) -> String {
    fs::read_to_string(&rig.params.flags_file).unwrap()
}

/// A decoy copy where the swapped path leads: a `kept` with an index, and a `kept.making` with
/// a file, each of which a removal by path would take.
fn decoys(swap: &Swap) {
    write(&swap.elsewhere.join("kept/index.json"), DECOY, 0o600);
    write(&swap.elsewhere.join("kept.making/0"), DECOY, 0o600);
}

/// The decoys are there as they were put.
fn decoys_untouched(swap: &Swap) {
    assert_eq!(swap.elsewhere(), ["kept", "kept.making"]);
    for file in ["kept/index.json", "kept.making/0"] {
        assert_eq!(
            fs::read_to_string(swap.elsewhere.join(file)).unwrap(),
            DECOY
        );
    }
}

/// Asserts that, with the state directory swapped for a link after each open, `prepare` makes
/// the copy in the directory it opened (the index and the flags file's bytes there, nothing at
/// the link's target), and the update's `rollback` reads that copy, puts the flags file back as
/// it was, and removes the copy from the directory it opened, the link's target still empty.
///
/// Catches a copy made, read or removed at the state directory's path after the check: a
/// `prepare` that writes through the link, and a `rollback` that finds no copy there and leaves
/// the package's flags in place.
#[test]
fn a_state_directory_swapped_for_a_link_after_the_open_redirects_neither_the_keep_nor_the_put_back()
{
    let rig = Rig::update("config-kept-swapped-rollback");
    let swap = Swap::new(&rig);
    write(&rig.params.flags_file, BEFORE, 0o600);
    write(&rig.staged.join("tree/flags"), AFTER, 0o644);

    let done = swap.prepare(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(names(&kept(&rig)), ["0", "index.json"]);
    assert_eq!(fs::read_to_string(kept(&rig).join("0")).unwrap(), BEFORE);
    assert!(swap.elsewhere().is_empty());

    install(&rig);
    rig.lease.say(&[Say::Host("live", Some(1000))]);
    assert_eq!(rig.run(cutover).outcome, Outcome::Success);

    let done = swap.run(&rig, rollback);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(flags_text(&rig), BEFORE);
    assert_eq!(
        fs::metadata(&rig.params.flags_file)
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o600
    );
    assert!(!kept(&rig).exists());
    assert!(!rig.params.state_dir.join("kept.making").exists());
    assert!(swap.elsewhere().is_empty());
}

/// Asserts that, with the state directory swapped for a link after the open, a `verify` that
/// sees the hello removes the copy from the directory it opened and leaves a `kept` and a
/// `kept.making` at the link's target as they are.
///
/// Catches a removal that follows the path after the check: the recursive remove and the
/// rename taking what the link leads to, and the real copy left in place.
#[test]
fn a_verify_with_the_state_directory_swapped_removes_only_the_copy_it_opened() {
    let rig = Rig::new("config-kept-swapped-verify");
    let swap = Swap::new(&rig);
    write(&rig.params.flags_file, BEFORE, 0o600);
    write(&rig.staged.join("tree/flags"), AFTER, 0o644);
    assert_eq!(rig.prepare().outcome, Outcome::Success);
    install(&rig);
    rig.lease.say(&[Say::Host("live", Some(1000))]);
    assert_eq!(rig.run(cutover).outcome, Outcome::Success);
    decoys(&swap);

    rig.lease.say(&[Say::Host("live", Some(2000))]);
    let done = swap.run(&rig, verify);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(done.report, Default::default());
    assert!(!kept(&rig).exists());
    assert!(!rig.params.state_dir.join("kept.making").exists());
    assert_eq!(flags_text(&rig), AFTER);
    decoys_untouched(&swap);
}

/// Asserts that, with the state directory swapped for a link after the open, a `cleanup` after
/// a `verify` that succeeded removes the copy from the directory it opened and the record, and
/// leaves a `kept` and a `kept.making` at the link's target as they are.
///
/// Catches the cleanup's removal of the copy by path after the check.
#[test]
fn a_cleanup_with_the_state_directory_swapped_removes_only_the_copy_it_opened() {
    let rig = Rig::new("config-kept-swapped-cleanup");
    let swap = Swap::new(&rig);
    write(&rig.params.flags_file, BEFORE, 0o600);
    write(&rig.staged.join("tree/flags"), AFTER, 0o644);
    assert_eq!(rig.prepare().outcome, Outcome::Success);
    rig.put(&Record {
        session_read: true,
        restart_begun: true,
        restarted: true,
        verified: true,
        ..rig.fresh()
    });
    decoys(&swap);

    let done = swap.run(&rig, cleanup);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert!(!kept(&rig).exists());
    assert!(rig.record().is_none());
    decoys_untouched(&swap);
}

/// Asserts that a `kept.making` a killed step left, with a directory in it and a link in it to
/// a directory outside the state directory, is removed whole by the next `prepare`, which then
/// keeps its copy: the link is removed and not followed, so what it leads to stays.
///
/// Catches a removal that stops at a directory in the copy (and so a `prepare` that fails
/// `keep_failed` on its own leftover), and one that follows a link in the copy and removes what
/// it leads to.
#[test]
fn prepare_removes_a_leftover_copy_with_a_directory_and_a_link_in_it_without_following_the_link() {
    let rig = Rig::new("config-kept-leftover-tree");
    write(&rig.params.flags_file, BEFORE, 0o600);
    write(&rig.staged.join("tree/flags"), AFTER, 0o644);
    let making = rig.params.state_dir.join("kept.making");
    write(&making.join("0"), BEFORE, 0o600);
    write(&making.join("deeper/inner/1"), BEFORE, 0o600);
    let outside = rig.root.join("outside");
    write(&outside.join("keep-me"), DECOY, 0o644);
    symlink(&outside, making.join("deeper/out")).unwrap();

    let done = rig.prepare();
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert!(!making.exists());
    assert_eq!(names(&kept(&rig)), ["0", "index.json"]);
    assert_eq!(fs::read_to_string(outside.join("keep-me")).unwrap(), DECOY);
}
