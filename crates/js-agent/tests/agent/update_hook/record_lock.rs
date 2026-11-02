//! The two locks of the hook's state directory (`Held`): the contract's lock on the directory
//! itself and the lock file inside it. A lock belongs to an open file description, so a
//! duplicate of the directory's descriptor that outlives the step would keep the directory
//! locked unless the step unlocks it itself; these tests hold such a duplicate, taken through
//! the stat seam, to see that it does, on success and on every way the lock file can fail.

use std::cell::RefCell;
use std::fs::{self, File};
use std::io;
use std::os::fd::{BorrowedFd, OwnedFd};
use std::os::unix::fs::symlink;
use std::path::Path;

use js_agent::update_hook::{DirStat, Held, LOCK_FILE, StateError};
use js_testkit::dir::TestDir;

/// A stat of the hook's seam that answers as root does and keeps a duplicate of the descriptor
/// of `dir`, the state directory, when the walk reaches it.
struct Keeping<'a> {
    dir: &'a Path,
    kept: RefCell<Vec<OwnedFd>>,
}

impl Keeping<'_> {
    fn stat(&self, path: &Path, fd: BorrowedFd<'_>) -> io::Result<DirStat> {
        if path == self.dir {
            self.kept.borrow_mut().push(fd.try_clone_to_owned()?);
        }
        Ok(DirStat {
            is_symlink: false,
            is_dir: true,
            uid: 0,
            gid: 0,
            mode: 0o700,
        })
    }

    fn open(&self) -> Result<Held, StateError> {
        Held::open(self.dir, &|path, fd| self.stat(path, fd))
    }

    fn kept(&self) -> usize {
        self.kept.borrow().len()
    }
}

/// Puts something other than a regular file at a path.
type Put = dyn Fn(&Path);

/// Whether a holder other than the step can take the directory's lock now.
fn directory_free(dir: &Path) -> bool {
    File::open(dir).unwrap().try_lock().is_ok()
}

/// Released by the step itself: with a duplicate of its descriptor still open, the directory is
/// free once the step's `Held` is dropped. Catches a release that relies on closing the
/// descriptor.
#[test]
fn the_directory_lock_is_released_by_the_step_even_with_its_descriptor_still_open() {
    let tmp = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "hook-lock-dup");
    let keeping = Keeping {
        dir: tmp.path(),
        kept: RefCell::new(Vec::new()),
    };
    let held = keeping.open().unwrap();
    assert_eq!(keeping.kept(), 1);
    assert!(!directory_free(tmp.path()), "the step holds the directory");
    drop(held);
    assert!(
        directory_free(tmp.path()),
        "a duplicate of the descriptor is open, and the directory is free"
    );
}

/// While the step holds both locks, a hook that takes only the lock file (one built before the
/// directory lock) cannot take it: the old hook is kept out by the new.
#[test]
fn a_hook_that_takes_only_the_lock_file_is_kept_out_while_the_step_holds_both() {
    let tmp = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "hook-lock-legacy");
    let keeping = Keeping {
        dir: tmp.path(),
        kept: RefCell::new(Vec::new()),
    };
    let held = keeping.open().unwrap();
    let old = File::options()
        .write(true)
        .open(tmp.path().join(LOCK_FILE))
        .unwrap();
    assert!(old.try_lock().is_err(), "the lock file is held");
    drop(held);
    old.try_lock().unwrap();
}

/// A lock file that cannot be opened as one (a directory, a symbolic link or a FIFO in its
/// place) fails the step without following or waiting, and the directory's lock is released
/// on that path too, with a duplicate of the descriptor still open.
#[test]
fn a_lock_file_that_cannot_be_opened_fails_the_step_and_releases_the_directory() {
    let make: [(&str, &Put); 3] = [
        ("dir", &|path: &Path| fs::create_dir(path).unwrap()),
        ("link", &|path: &Path| {
            symlink("/nonexistent-target", path).unwrap();
        }),
        ("fifo", &|path: &Path| {
            rustix::fs::mkfifoat(
                rustix::fs::CWD,
                path,
                rustix::fs::Mode::from_raw_mode(0o600),
            )
            .unwrap();
        }),
    ];
    for (name, put) in make {
        let tmp = TestDir::under(env!("CARGO_TARGET_TMPDIR"), &format!("hook-lock-{name}"));
        put(&tmp.path().join(LOCK_FILE));
        let keeping = Keeping {
            dir: tmp.path(),
            kept: RefCell::new(Vec::new()),
        };
        let error = keeping.open().unwrap_err();
        assert!(
            !matches!(error, StateError::Lock { .. }),
            "{name}: not a second holder: {error:?}"
        );
        assert_eq!(keeping.kept(), 1, "{name}");
        assert!(
            directory_free(tmp.path()),
            "{name}: the directory is released"
        );
    }
}
