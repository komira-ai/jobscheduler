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

/// Asserts that when the rename of a rewrite fails, a reader of the file still gets the
/// old document whole, leases and reservations together: one replace carries both, so
/// no record appears without the leases it was written with.
#[test]
fn a_failed_rename_keeps_old_leases_with_old_reservations() {
    let dir = Dir::new("document");
    let path = dir.file();
    let text = |term: u32, give_ups: u32| {
        format!(
            "version = 1\nterm = {term}\nnext_sequence = 2\nlease_count = 1\nreservation_count = 1\n\n\
             [[lease]]\nterm = {term}\nsequence = 1\nstate = \"held\"\nacquired_at_ms = 1\n\
             expires_at_ms = 2\nends_at_ms = 3\n\n\
             [[reservation]]\nrequest_id = \"r\"\ngive_ups = {give_ups}\n"
        )
    };
    replace(&path, text(1, 1).as_bytes()).unwrap();
    replace_with(&path, text(2, 2).as_bytes(), refuse).unwrap_err();
    let document = super::read(&path).unwrap().unwrap();
    assert_eq!((document.term, document.lease[0].term), (1, 1));
    assert_eq!(document.reservation[0].give_ups, 1);
}
