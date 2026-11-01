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

/// What a build that predates the wait-list place and the assigned flag reads a
/// reservation row as: the old fields, and everything else kept apart. It is the
/// reader of a lease file as it was before those fields, copied here as it was.
#[derive(serde::Deserialize)]
struct OlderRow {
    request_id: String,
    host: Option<String>,
    deadline_ms: Option<u64>,
    #[serde(default)]
    give_ups: u32,
    #[serde(flatten)]
    extra: toml::Table,
}

#[derive(serde::Deserialize)]
struct OlderDocument {
    term: u64,
    lease_count: u64,
    #[serde(default)]
    reservation: Vec<OlderRow>,
    #[serde(flatten)]
    extra: toml::Table,
}

/// Asserts that a file with the new row keys is read by an older reader (a struct that
/// names only the old fields and keeps the rest, as every reader of this file does):
/// it parses, the old fields read as written, and the three new keys land in the
/// kept-apart table to be written back. It is the skip rule the format relies on: a
/// build that does not know a key reads past it and does not refuse the file.
#[test]
fn an_older_reader_skips_the_wait_place_and_the_assigned_flag() {
    let text = "version = 1\nterm = 4\nnext_sequence = 1\nlease_count = 0\nreservation_count = 1\n\n\
        [[reservation]]\nrequest_id = \"r\"\nhost = \"h1\"\nreserved_at_ms = 1\ndeadline_ms = 2\n\
        assigned = true\nqueue_place = 3\nqueued_since_ms = 9\ngive_ups = 1\n";
    let older: OlderDocument = toml::from_str(text).unwrap();
    assert_eq!((older.term, older.lease_count), (4, 0));
    assert!(older.extra.contains_key("reservation_count"));
    let row = &older.reservation[0];
    assert_eq!(
        (
            row.request_id.as_str(),
            row.host.as_deref(),
            row.deadline_ms,
            row.give_ups
        ),
        ("r", Some("h1"), Some(2), 1)
    );
    for key in ["assigned", "queue_place", "queued_since_ms"] {
        assert!(row.extra.contains_key(key), "{key}");
    }
    // The current build reads the same text whole.
    let current: super::Document = toml::from_str(text).unwrap();
    let new = &current.reservation[0];
    assert_eq!(
        (new.assigned, new.queue_place, new.queued_since_ms),
        (true, Some(3), Some(9))
    );
}

/// Asserts that a row with `tail_end_ms` is read by the older reader above, which keeps
/// the key apart and refuses nothing, and by the current build whole: the skip rule for
/// the end of the tail a mark was made with.
#[test]
fn an_older_reader_skips_the_tail_end() {
    let text = "version = 1\nterm = 4\nnext_sequence = 1\nlease_count = 0\nreservation_count = 1\n\n\
        [[reservation]]\nrequest_id = \"r\"\nhost = \"h1\"\nreserved_at_ms = 1\ndeadline_ms = 2\n\
        tail_end_ms = 77\ngive_ups = 1\n";
    let older: OlderDocument = toml::from_str(text).unwrap();
    let row = &older.reservation[0];
    assert_eq!((row.request_id.as_str(), row.deadline_ms), ("r", Some(2)));
    assert!(row.extra.contains_key("tail_end_ms"));
    let current: super::Document = toml::from_str(text).unwrap();
    assert_eq!(current.reservation[0].tail_end_ms, Some(77));
}
