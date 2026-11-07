//! What the state tests share: a record type of their own (schema 3, so a newer and an older
//! record are distinct from the one js-agent writes), the fake `stat` of the open seam, and
//! the helpers that put odd things where the state directory's files go.
//!
//! Each test binary that includes this uses some of it, so the unused rest is not an error:
//! the allow below is the stated reason, one helper set for two binaries.
#![allow(dead_code)]

use std::fs;
use std::io;
use std::os::fd::BorrowedFd;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use js_testkit::dir::TestDir;
use js_update_hook::{DirStat, Document, Held, ReasonToken, StateError};
use serde::{Deserialize, Serialize};

/// The record the tests keep: schema 3, nothing else the state code cares about.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Note {
    pub schema: u32,
    pub name: String,
}

impl Note {
    pub fn new(name: &str) -> Self {
        Self {
            schema: 3,
            name: name.to_owned(),
        }
    }
}

/// The hook's own rule that a record broke.
#[derive(Debug, PartialEq, Eq)]
pub struct NoteInvalid(pub &'static str);

impl std::fmt::Display for NoteInvalid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for NoteInvalid {}

/// The test hook's reasons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestReason {
    Locked,
    StateInvalid,
}

impl ReasonToken for TestReason {
    fn as_str(self) -> &'static str {
        match self {
            TestReason::Locked => "locked",
            TestReason::StateInvalid => "state_invalid",
        }
    }
}

impl Document for Note {
    const SCHEMA: u32 = 3;
    type Invalid = NoteInvalid;
    type Reason = TestReason;
    const LOCKED: TestReason = TestReason::Locked;
    const STATE_INVALID: TestReason = TestReason::StateInvalid;
}

/// A record that cannot be written out: its serialisation always fails.
#[derive(Debug, Deserialize)]
pub struct Unwritable {
    pub schema: u32,
}

impl Serialize for Unwritable {
    fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
        Err(serde::ser::Error::custom("cannot serialize"))
    }
}

impl Document for Unwritable {
    const SCHEMA: u32 = 3;
    type Invalid = NoteInvalid;
    type Reason = TestReason;
    const LOCKED: TestReason = TestReason::Locked;
    const STATE_INVALID: TestReason = TestReason::StateInvalid;
}

/// What the directory of a root-run hook looks like.
pub const ROOT_0700: DirStat = DirStat {
    is_symlink: false,
    is_dir: true,
    uid: 0,
    gid: 0,
    mode: 0o700,
};

/// A `stat` that says every directory is as it must be, whoever runs the test.
pub fn as_root(_: &Path, _: BorrowedFd<'_>) -> io::Result<DirStat> {
    Ok(ROOT_0700)
}

/// A `stat` that answers `seen` for `target` and as root does for every other path.
pub fn seeing(
    target: &Path,
    seen: DirStat,
) -> impl Fn(&Path, BorrowedFd<'_>) -> io::Result<DirStat> {
    let target = target.to_owned();
    move |path, _| Ok(if path == target { seen } else { ROOT_0700 })
}

/// A `stat` that fails with `error` at `target` and as root does elsewhere.
pub fn failing_at(target: &Path) -> impl Fn(&Path, BorrowedFd<'_>) -> io::Result<DirStat> {
    let target = target.to_owned();
    move |path, _| {
        if path == target {
            Err(io::Error::other("stat refused"))
        } else {
            Ok(ROOT_0700)
        }
    }
}

/// A `stat` that shows what is at and under `tree` as it really is, except that root owns
/// it (the test is not root), and shows everything above it as a root-owned 0755 directory.
pub fn rooted(tree: &Path) -> impl Fn(&Path, BorrowedFd<'_>) -> io::Result<DirStat> {
    let tree = tree.to_owned();
    move |path, fd| {
        if path.starts_with(&tree) {
            Ok(DirStat {
                uid: 0,
                gid: 0,
                ..js_update_hook::fd_stat(fd)?
            })
        } else {
            Ok(DirStat {
                mode: 0o755,
                ..ROOT_0700
            })
        }
    }
}

/// A scratch directory of its own.
pub fn cell(name: &str) -> TestDir {
    TestDir::under(env!("CARGO_TARGET_TMPDIR"), name)
}

/// `dir` opened as a root-run hook opens its state directory.
pub fn open(dir: &Path) -> Result<Held<Note>, StateError<Note>> {
    Held::open(dir, &as_root)
}

/// The record file of `dir`, written with `body` as it is.
pub fn write_record(dir: &Path, body: &str) {
    fs::write(dir.join(js_update_hook::RECORD_FILE), body).unwrap();
}

/// A record of schema `schema` with the name "kept".
pub fn record_of(schema: u32) -> String {
    format!("{{\"schema\": {schema}, \"name\": \"kept\"}}")
}

/// A directory of the mode `mode`, made at `path`.
pub fn dir_with_mode(path: &Path, mode: u32) {
    fs::create_dir_all(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

/// A named pipe at `path`.
pub fn fifo(path: &Path) -> PathBuf {
    nix::unistd::mkfifo(
        path,
        nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
    )
    .unwrap();
    path.to_owned()
}
