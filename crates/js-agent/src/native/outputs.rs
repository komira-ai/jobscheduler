//! The declared outputs of a native run (`NativeSpec::outputs`), copied out of the run's
//! working directory when the run is removed: after `kill-uid` (no process of the run's
//! user is left to write or to swap a name) and before `user-delete` (which sweeps the home
//! they are in).
//!
//! The agent copies as root from a directory its job owned, so it never follows a link: every
//! name from the home down is looked at without following it (`AT_SYMLINK_NOFOLLOW`) and
//! opened with `O_NOFOLLOW`, and a link anywhere is a failure. A `file` output is copied when
//! it is a regular file, and a `dir` output as the regular files of its tree; a link, a
//! device, a socket or a pipe in a tree is not copied and is a failure, and the rest of the
//! tree is copied. A regular file with more than one name (a hard link, which a job may make
//! to a file it cannot read) is not copied either. One file may hold at most the run's
//! largest file (its `RLIMIT_FSIZE`), and the run's outputs together at most
//! [`MAX_COLLECTED_BYTES`]; a file over either is not copied past it and is a failure. The
//! trees of a run hold at most [`MAX_ENTRIES`] entries, and a removal's error names at most
//! sixteen failures. An output the job did not make is not an error.
//!
//! The copies go to `outputs/<path>` in the run's scratch directory, which is the agent's,
//! files `0600` and directories `0700`. What the collection says names paths and counts
//! bytes, and never holds what a file contains.

use std::ffi::{CStr, CString, OsStr};
use std::fmt::Display;
use std::fs::{self, File};
use std::io::{self, Read as _};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

use js_agent_wire::{OutputKind, OutputSpec};
use rustix::fs::{AtFlags, CWD, Dir, FileType, Mode, OFlags, fstat, openat, statat};
use rustix::io::Errno;

/// The most bytes the outputs of one run may hold together (1 GiB).
pub(super) const MAX_COLLECTED_BYTES: u64 = 1 << 30;

/// The most entries the directory outputs of one run may hold together; the walk of a tree
/// stops at the first entry past it.
pub(super) const MAX_ENTRIES: usize = 100_000;

/// The most failures a removal's error names; the others are counted.
const MAX_TOLD: usize = 16;

/// The deepest a directory output's tree may go; a deeper directory is not copied.
const MAX_DEPTH: usize = 64;

const DIR_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

const FILE_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// What the collection of one run's outputs did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct Collected {
    /// Each file copied: its path below the working directory and how many bytes it holds.
    pub(super) copied: Vec<(String, u64)>,
    /// Each path that could not be copied, with why: never what it holds.
    pub(super) failed: Vec<String>,
}

/// The caps of one run's collection.
#[derive(Debug, Clone, Copy)]
pub(super) struct Caps {
    /// The most bytes of one file (the run's largest file).
    pub(super) file_bytes: u64,
    /// The most bytes of all the run's outputs ([`MAX_COLLECTED_BYTES`]).
    pub(super) run_bytes: u64,
    /// The most entries of all the run's directory outputs ([`MAX_ENTRIES`]).
    pub(super) entries: usize,
}

/// What may still be copied: the bytes of one file, the bytes and the tree entries left for
/// the run.
struct Budget {
    file: u64,
    left: u64,
    entries: usize,
}

impl Budget {
    /// Takes one tree entry from the run's budget.
    ///
    /// # Errors
    ///
    /// None is left: the walk stops.
    fn take_entry(&mut self) -> Result<(), String> {
        if self.entries == 0 {
            return Err(
                "over the entries the outputs of a run may hold: the rest is not copied".to_owned(),
            );
        }
        self.entries -= 1;
        Ok(())
    }
}

/// Copies the outputs `outputs` of a run whose home is `home` (its working directory is
/// `home/exec`) into `into`, within `caps`.
pub(super) fn collect(home: &Path, outputs: &[OutputSpec], into: &Path, caps: Caps) -> Collected {
    let mut done = Collected::default();
    let mut budget = Budget {
        file: caps.file_bytes,
        left: caps.run_bytes,
        entries: caps.entries,
    };
    for output in outputs {
        if let Err(why) = one(home, output, into, &mut budget, &mut done) {
            done.failed.push(format!("{}: {why}", output.path));
        }
    }
    done
}

/// Copies one output; a failure of the output as a whole is the error, and a failure of a
/// file in a directory output is noted in `done`.
fn one(
    home: &Path,
    output: &OutputSpec,
    into: &Path,
    budget: &mut Budget,
    done: &mut Collected,
) -> Result<(), String> {
    let parts = plain_parts(&output.path)?;
    let Some((dir, name)) = reach(home, &parts)? else {
        return Ok(());
    };
    let found = kind(&dir, &name)?;
    let to = into.join(&output.path);
    match (output.kind, found) {
        (_, None) => Ok(()),
        (OutputKind::File, Some(FileType::RegularFile)) => {
            make_dir(to.parent().unwrap_or(into))?;
            let bytes = copy(&dir, &name, &to, budget)?;
            done.copied.push((output.path.clone(), bytes));
            Ok(())
        }
        (OutputKind::Dir, Some(FileType::Directory)) => {
            let tree = openat(&dir, name.as_c_str(), DIR_FLAGS, Mode::empty()).map_err(said)?;
            copy_tree(&tree, (&output.path, &to), 1, budget, done)
        }
        (_, Some(FileType::Symlink)) => Err("a symbolic link: not followed".to_owned()),
        (OutputKind::File, Some(_)) => Err("not a regular file".to_owned()),
        (OutputKind::Dir, Some(_)) => Err("not a directory".to_owned()),
    }
}

/// The names of `path`, which must be relative and made of plain names (no empty name, no
/// `.` or `..`, no NUL).
fn plain_parts(path: &str) -> Result<Vec<CString>, String> {
    let plain = |part: &&str| !matches!(*part, "" | "." | "..");
    let parts: Vec<&str> = path.split('/').collect();
    match parts.iter().all(plain) {
        true => parts
            .into_iter()
            .map(|part| CString::new(part).map_err(said))
            .collect(),
        false => Err("not a relative path of plain names".to_owned()),
    }
}

/// Walks from `home` down through `exec` and every name of `parts` but the last, each a
/// directory and not a link, and returns the directory the last name is in with that name;
/// none when a name on the way is missing (the job did not make the output).
fn reach(home: &Path, parts: &[CString]) -> Result<Option<(OwnedFd, CString)>, String> {
    let mut dir = match openat(CWD, home, DIR_FLAGS, Mode::empty()) {
        Err(Errno::NOENT) => return Ok(None),
        opened => opened.map_err(said)?,
    };
    let (last, above) = parts.split_last().ok_or("an empty path")?;
    let exec = c"exec".to_owned();
    for part in std::iter::once(&exec).chain(above) {
        match kind(&dir, part)? {
            None => return Ok(None),
            Some(FileType::Directory) => {
                dir = openat(&dir, part.as_c_str(), DIR_FLAGS, Mode::empty()).map_err(said)?;
            }
            Some(other) => return Err(format!("{} on the way to it", what(other))),
        }
    }
    Ok(Some((dir, last.clone())))
}

/// The kind of the entry `name` of `dir`, not following a link; none when it is missing.
fn kind(dir: &OwnedFd, name: &CStr) -> Result<Option<FileType>, String> {
    let found = statat(dir, name, AtFlags::SYMLINK_NOFOLLOW);
    match found.map(|stat| FileType::from_raw_mode(stat.st_mode)) {
        Err(Errno::NOENT) => Ok(None),
        other => other.map(Some).map_err(said),
    }
}

/// How a kind of entry that is not where it is wanted is told.
fn what(kind: FileType) -> &'static str {
    match kind {
        FileType::Symlink => "a symbolic link",
        FileType::RegularFile => "a regular file",
        _ => "a special file",
    }
}

/// Copies the tree of the directory `dir`, the output's `rel` (its path below the working
/// directory), into `to`. Each entry that cannot be copied is noted in `done`, and the
/// others are still copied.
fn copy_tree(
    dir: &OwnedFd,
    (rel, to): (&str, &Path),
    depth: usize,
    budget: &mut Budget,
    done: &mut Collected,
) -> Result<(), String> {
    if depth > MAX_DEPTH {
        return Err(format!("deeper than {MAX_DEPTH} directories"));
    }
    make_dir(to)?;
    let mut names: Vec<CString> = Dir::read_from(dir)
        .map_err(said)?
        .flatten()
        .map(|entry| entry.file_name().to_owned())
        .filter(|name| !matches!(name.to_bytes(), b"." | b".."))
        .collect();
    names.sort();
    for name in names {
        budget.take_entry()?;
        let path = format!("{rel}/{}", name.to_string_lossy());
        let target = to.join(OsStr::from_bytes(name.to_bytes()));
        // Listed a moment ago and no process of the user is left: a name that is gone now
        // is an error like any other.
        let found = statat(dir, name.as_c_str(), AtFlags::SYMLINK_NOFOLLOW).map_err(said)?;
        let copied = match FileType::from_raw_mode(found.st_mode) {
            FileType::RegularFile => copy(dir, &name, &target, budget)
                .map(|bytes| done.copied.push((path.clone(), bytes))),
            FileType::Directory => openat(dir, name.as_c_str(), DIR_FLAGS, Mode::empty())
                .map_err(said)
                .and_then(|sub| copy_tree(&sub, (&path, &target), depth + 1, budget, done)),
            other => Err(format!("{}: not copied", what(other))),
        };
        if let Err(why) = copied {
            done.failed.push(format!("{path}: {why}"));
        }
    }
    Ok(())
}

/// Copies the regular file `name` of `dir` to `to`, a new file, within `budget`. A file over
/// the budget is not kept: the part copied is removed again. A file with more than one name
/// is not copied: a job may give a second name to a file it cannot read (macOS does not stop
/// it), and the agent reads as root.
fn copy(dir: &OwnedFd, name: &CStr, to: &Path, budget: &mut Budget) -> Result<u64, String> {
    let opened = openat(dir, name, FILE_FLAGS, Mode::empty()).map_err(said)?;
    let names = fstat(&opened).map_err(said)?.st_nlink;
    if names != 1 {
        return Err(format!("a file with {names} names: not copied"));
    }
    let file = File::from(opened);
    let limit = budget.file.min(budget.left);
    let mut out = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(to)
        .map_err(said)?;
    let copied = io::copy(&mut file.take(limit.saturating_add(1)), &mut out).map_err(said)?;
    if copied > limit {
        drop(out);
        let _ = fs::remove_file(to);
        return Err(format!("over the {limit} bytes it may hold: not copied"));
    }
    budget.left -= copied;
    Ok(copied)
}

/// Makes the directory `path` and those above it that are missing, `0700`.
fn make_dir(path: &Path) -> Result<(), String> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(said)
}

/// The failures `failed` as one text: the first [`MAX_TOLD`], and how many more there are.
pub(super) fn told(failed: &[String]) -> String {
    let mut text = failed
        .iter()
        .take(MAX_TOLD)
        .cloned()
        .collect::<Vec<_>>()
        .join("; ");
    if failed.len() > MAX_TOLD {
        text.push_str(&format!("; and {} more", failed.len() - MAX_TOLD));
    }
    text
}

/// The text of an error.
fn said(why: impl Display) -> String {
    why.to_string()
}

/// Where the outputs of a run whose scratch directory is `scratch` are copied to.
pub(super) fn dir_in(scratch: &Path) -> PathBuf {
    scratch.join("outputs")
}

#[cfg(test)]
mod tests {
    use js_testkit::dir::TestDir;

    use super::*;

    /// Catches a run whose outputs together pass the run's budget: with room for five bytes
    /// in all, the first output of four bytes is copied, the second of four is not (only one
    /// byte is left) and no part of it is kept, and a third of one byte still fits.
    #[test]
    fn the_outputs_of_a_run_share_one_budget() {
        let dir = TestDir::under(std::env::temp_dir(), "native-outputs-budget");
        let home = dir.path().join("home");
        fs::create_dir_all(home.join("exec")).unwrap();
        for (name, text) in [("a", "1234"), ("b", "5678"), ("c", "9")] {
            fs::write(home.join("exec").join(name), text).unwrap();
        }
        let outputs: Vec<OutputSpec> = ["a", "b", "c"]
            .iter()
            .map(|path| OutputSpec {
                path: (*path).to_owned(),
                kind: OutputKind::File,
            })
            .collect();
        let into = dir.path().join("outputs");
        let caps = Caps {
            file_bytes: 100,
            run_bytes: 5,
            entries: MAX_ENTRIES,
        };
        let done = collect(&home, &outputs, &into, caps);
        let copied = vec![("a".to_owned(), 4), ("c".to_owned(), 1)];
        assert_eq!(done.copied, copied);
        assert_eq!(done.failed, ["b: over the 1 bytes it may hold: not copied"]);
        assert_eq!(fs::read(into.join("a")).unwrap(), b"1234");
        assert!(!into.join("b").exists(), "a part of b was kept");
        assert_eq!(fs::read(into.join("c")).unwrap(), b"9");
    }

    /// Catches a tree walked without a bound on its breadth: with room for three entries, a
    /// directory output of five files has the first three (by name) copied and one failure
    /// that says the rest was not, and no more.
    #[test]
    fn the_entries_of_a_run_are_bounded() {
        let dir = TestDir::under(std::env::temp_dir(), "native-outputs-entries");
        let home = dir.path().join("home");
        fs::create_dir_all(home.join("exec/tree")).unwrap();
        for name in ["a", "b", "c", "d", "e"] {
            fs::write(home.join("exec/tree").join(name), name).unwrap();
        }
        let outputs = [OutputSpec {
            path: "tree".to_owned(),
            kind: OutputKind::Dir,
        }];
        let into = dir.path().join("outputs");
        let caps = Caps {
            file_bytes: 100,
            run_bytes: 100,
            entries: 3,
        };
        let done = collect(&home, &outputs, &into, caps);
        let names: Vec<&str> = done.copied.iter().map(|(path, _)| path.as_str()).collect();
        assert_eq!(names, ["tree/a", "tree/b", "tree/c"]);
        let why = "tree: over the entries the outputs of a run may hold: the rest is not copied";
        assert_eq!(done.failed, [why]);
        assert!(!into.join("tree/d").exists());
    }

    /// Catches an error that grows with what the job left: sixteen failures are told in
    /// full, and of twenty the first sixteen are told and the other four counted.
    #[test]
    fn a_removal_tells_sixteen_failures_and_counts_the_rest() {
        let failed: Vec<String> = (0..20).map(|n| format!("f{n}: no")).collect();
        let sixteen = (0..16)
            .map(|n| format!("f{n}: no"))
            .collect::<Vec<_>>()
            .join("; ");
        assert_eq!(told(&failed[..16]), sixteen);
        assert_eq!(told(&failed), format!("{sixteen}; and 4 more"));
    }
}
