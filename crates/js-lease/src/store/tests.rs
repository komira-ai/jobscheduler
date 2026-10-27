//! The write points of [`super::replace`]: what is on disk when the rename fails.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::{replace, replace_with, sibling};

/// A directory next to the test binary, emptied when the test ends.
struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Self {
        let exe = std::env::current_exe().unwrap();
        let dir = exe.parent().unwrap().join(format!("js-lease-store-{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn file(&self) -> PathBuf {
        self.0.join("leases.toml")
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn refuse(_from: &Path, _to: &Path) -> io::Result<()> {
    Err(io::Error::other("rename refused"))
}

/// Asserts that a rename that fails after the new bytes were written and synced to the
/// temporary file leaves the old file byte for byte what a reader gets, reports the
/// error, and that the next replace overwrites the left-over temporary file and renames
/// it into place.
#[test]
fn a_failed_rename_leaves_the_old_file_intact() {
    let dir = Dir::new("rename");
    let path = dir.file();
    replace(&path, b"old\n").unwrap();

    let err = replace_with(&path, b"new\n", refuse).unwrap_err();
    assert_eq!(err.to_string(), "rename refused");
    assert_eq!(fs::read(&path).unwrap(), b"old\n");
    assert_eq!(fs::read(sibling(&path, ".tmp")).unwrap(), b"new\n");

    replace(&path, b"newer\n").unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"newer\n");
    assert!(!sibling(&path, ".tmp").exists());
}

/// Asserts that a failed rename of the very first write leaves no state file at all, so
/// the next open still sees a missing file and not a half-written one.
#[test]
fn a_failed_first_rename_leaves_no_state_file() {
    let dir = Dir::new("first");
    let path = dir.file();
    replace_with(&path, b"new\n", refuse).unwrap_err();
    assert!(!path.exists());
}
