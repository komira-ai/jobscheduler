//! Making a file that must not already exist, with its whole content or not at all.

use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{self, ErrorKind, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// The step of [`create_new`], [`check_file_path`] or [`probe_dir`] that failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum NewFileStep {
    /// The path names no file: it is empty, ends in a separator, or ends in `.` or `..`.
    #[error("use")]
    Path,
    /// No random bytes were available for a name.
    #[error("name a new file beside")]
    Entropy,
    /// The directory could not be examined.
    #[error("examine")]
    Inspect,
    /// The directory is one where another user could swap the file that was staged.
    #[error("use")]
    Unsafe,
    /// The staged file could not be created, or every name tried was taken.
    #[error("create")]
    Stage,
    /// The mode of the staged file could not be set.
    #[error("set the mode of")]
    Permissions,
    /// The content could not be written to the staged file.
    #[error("write")]
    Write,
    /// The staged file could not be synced.
    #[error("sync")]
    Sync,
    /// The staged file could not be linked to the destination; something may be there.
    #[error("link a staged file to")]
    Link,
    /// The probe could not make or link its files.
    #[error("make and link probe files in")]
    Probe,
}

/// Why a file was not made, or a directory cannot take one: the step, the path it acted on,
/// and the system's reason. The text holds the step and the path and never the content.
#[derive(Debug, thiserror::Error)]
#[error("cannot {step} \"{}\": {source}", .path.display())]
pub struct NewFileError {
    step: NewFileStep,
    path: PathBuf,
    source: io::Error,
}

impl NewFileError {
    /// The step that failed.
    #[must_use]
    pub fn step(&self) -> NewFileStep {
        self.step
    }

    /// The path the step acted on: the destination, a staged or probe name beside it, or
    /// the directory.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The kind of the underlying error, so that a caller can tell
    /// [`io::ErrorKind::AlreadyExists`] (with [`NewFileStep::Link`]: the destination is
    /// taken) from a failure of the system.
    #[must_use]
    pub fn kind(&self) -> ErrorKind {
        self.source.kind()
    }
}

/// A closure for the error of `step` on `path`.
fn at(step: NewFileStep, path: &Path) -> impl FnOnce(io::Error) -> NewFileError {
    let path = path.to_path_buf();
    move |source| NewFileError { step, path, source }
}

/// Makes a file name from a prefix: the source of the names of staged and probe files, which
/// a test replaces to make a name collide.
type Names<'a> = &'a mut dyn FnMut(&str) -> io::Result<String>;

/// How many names are tried for a staged file before the call gives up. A name taken is the
/// work of someone else (the names are 64 random bits), so it is skipped, never removed.
const ATTEMPTS: usize = 4;

/// Makes the file `path` with `bytes` in it and permission bits `mode`, if nothing is at
/// `path` yet. The bytes are written to a file beside it under a name made for this call
/// (`.js-new.<16 hex digits>`, the same length whatever the length of the name of `path`,
/// made with `O_EXCL`), synced, and hard-linked to `path`; the link fails if anything is
/// there, a dangling symbolic link included, so no reader ever sees a part of the content
/// and an existing file is never replaced. The staged file is made with `mode` (the umask
/// can only take bits off it), so it is never more open than the file will be.
///
/// Only a name this call made is removed. A staged name that is taken (by a file, a link or
/// a directory) is left as it is and another name is tried, up to four times in all; the
/// staged name this call made is removed whether or not the link was made.
///
/// Once the link is made the call has succeeded: the sync of the directory that follows is
/// best effort, because a directory that cannot be read cannot be synced and the file is in
/// place all the same.
///
/// The call does not check who can write the directory; [`probe_dir`] does.
///
/// # Errors
///
/// A [`NewFileError`] when any step up to the link failed; nothing is then at `path` that
/// this call made. Its kind is [`io::ErrorKind::AlreadyExists`] with the step
/// [`NewFileStep::Link`] when something is already at `path`. The text of an error is the
/// system's and never holds `bytes`. A crash between the staging and the removal leaves the
/// staged file, under its own name.
pub fn create_new(path: &Path, bytes: &[u8], mode: u32) -> Result<(), NewFileError> {
    create_new_with(path, bytes, mode, &mut unique)
}

/// [`create_new`] with the names of staged files drawn from `names`.
fn create_new_with(
    path: &Path,
    bytes: &[u8],
    mode: u32,
    names: Names<'_>,
) -> Result<(), NewFileError> {
    check_file_path(path)?;
    let (staged, file) = open_staged(path, mode, names)?;
    let made = fill(&staged, file, bytes, mode)
        .and_then(|()| fs::hard_link(&staged, path).map_err(at(NewFileStep::Link, path)));
    let _ = fs::remove_file(&staged);
    made.inspect(|()| {
        let _ = sync_parent(path);
    })
}

/// Whether `path` names a file that could be made: it is not empty, does not end in a
/// separator (which would name a directory) and does not end in `.` or `..`.
///
/// # Errors
///
/// A [`NewFileError`] of the step [`NewFileStep::Path`] saying which.
pub fn check_file_path(path: &Path) -> Result<(), NewFileError> {
    let why = match (path.file_name(), path.as_os_str().as_bytes()) {
        (None, _) => "it names no file",
        (Some(_), bytes) if bytes.ends_with(b"/") => "it ends in a separator",
        // `Path` drops a final `.` component, so `dir/.` would pass for `dir`.
        (Some(_), bytes) if bytes.ends_with(b"/.") => "it ends in `.`",
        (Some(_), _) => return Ok(()),
    };
    Err(at(NewFileStep::Path, path)(io::Error::new(
        ErrorKind::InvalidInput,
        why,
    )))
}

/// Whether a file can be made in `dir` and linked to another name there, which is all
/// [`create_new`] asks of a directory: found by doing it, under names made for the probe,
/// and removing the ones the probe made. What it cannot tell is a failure later (a full
/// disk, a directory changed in between).
///
/// A directory that group or others can write is refused unless it has the sticky bit (so
/// another user could swap the staged file before it is linked), and so is one owned by a
/// user other than the caller and root. A probe name that is taken is an error, not a retry,
/// and the entry there is left as it is.
///
/// # Errors
///
/// A [`NewFileError`] with the system's reason the directory cannot take [`create_new`], or
/// of the step [`NewFileStep::Unsafe`] when who can write it makes it unfit.
pub fn probe_dir(dir: &Path) -> Result<(), NewFileError> {
    probe_with(dir, &mut unique)
}

/// [`probe_dir`] with the names of the probe files drawn from `names`.
fn probe_with(dir: &Path, names: Names<'_>) -> Result<(), NewFileError> {
    let meta = fs::metadata(dir).map_err(at(NewFileStep::Inspect, dir))?;
    let caller = rustix::process::geteuid().as_raw();
    if let Some(why) = unfit(meta.mode(), meta.uid(), caller) {
        return Err(at(NewFileStep::Unsafe, dir)(io::Error::new(
            ErrorKind::PermissionDenied,
            why,
        )));
    }
    let first = dir.join(names(".js-probe").map_err(at(NewFileStep::Entropy, dir))?);
    let second = dir.join(names(".js-probe").map_err(at(NewFileStep::Entropy, dir))?);
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&first)
        .map_err(at(NewFileStep::Probe, &first))?;
    let linked = fs::hard_link(&first, &second).map_err(at(NewFileStep::Probe, &second));
    let _ = fs::remove_file(&first);
    if linked.is_ok() {
        let _ = fs::remove_file(&second);
    }
    linked
}

/// Why a directory of this mode and owner is unfit for a secret to be staged in, if it is:
/// the directory is some other user's, or group or others can write it and it is not sticky,
/// so another user could replace the staged file.
fn unfit(mode: u32, owner: u32, caller: u32) -> Option<String> {
    if owner != caller && owner != 0 {
        Some(format!(
            "it is owned by user {owner}, not by user {caller} or root"
        ))
    } else if mode & 0o022 != 0 && mode & 0o1000 == 0 {
        Some(format!(
            "its mode {:04o} lets group or others write there and it is not sticky",
            mode & 0o7777
        ))
    } else {
        None
    }
}

/// `<prefix>.<16 random hex digits>`.
fn unique(prefix: &str) -> io::Result<String> {
    let mut bits = [0u8; 8];
    getrandom::getrandom(&mut bits).map_err(io::Error::from)?;
    Ok(format!("{prefix}.{:016x}", u64::from_le_bytes(bits)))
}

/// Makes the staged file beside `path`, new, under a name from `names`.
fn open_staged(path: &Path, mode: u32, names: Names<'_>) -> Result<(PathBuf, File), NewFileError> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        let name = names(".js-new").map_err(at(NewFileStep::Entropy, path))?;
        let staged = path.with_file_name(name);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&staged)
        {
            Ok(file) => return Ok((staged, file)),
            Err(err) if err.kind() == ErrorKind::AlreadyExists && attempt < ATTEMPTS => {}
            Err(err) => return Err(at(NewFileStep::Stage, &staged)(err)),
        }
    }
}

/// Sets the mode of the staged file, writes `bytes` to it and syncs it.
fn fill(staged: &Path, mut file: File, bytes: &[u8], mode: u32) -> Result<(), NewFileError> {
    // The process's umask may have taken bits off the mode it was made with.
    file.set_permissions(Permissions::from_mode(mode))
        .map_err(at(NewFileStep::Permissions, staged))?;
    file.write_all(bytes)
        .map_err(at(NewFileStep::Write, staged))?;
    file.sync_all().map_err(at(NewFileStep::Sync, staged))
}

/// Makes the directory entry of `path` durable.
fn sync_parent(path: &Path) -> io::Result<()> {
    let dir = path.parent().filter(|dir| !dir.as_os_str().is_empty());
    File::open(dir.unwrap_or(Path::new("."))).and_then(|dir| dir.sync_all())
}

#[cfg(test)]
mod tests {
    use super::*;
    use js_testkit::dir::TestDir;
    use std::ffi::OsString;
    use std::os::unix::fs::symlink;

    /// A scratch directory only its owner can write, whatever the umask: a umask of 002
    /// would make it group-writable, which [`probe_dir`] refuses.
    fn private(name: &str) -> TestDir {
        let dir = TestDir::under(std::env::temp_dir(), name);
        fs::set_permissions(dir.path(), Permissions::from_mode(0o700)).unwrap();
        dir
    }

    fn listing(dir: &Path) -> Vec<OsString> {
        let mut names: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        names.sort();
        names
    }

    /// Names drawn from a list, in order, whatever the prefix.
    fn from_list<'a>(list: &'a [&'a str]) -> impl FnMut(&str) -> io::Result<String> + 'a {
        let mut next = list.iter();
        move |_| Ok((*next.next().unwrap()).to_owned())
    }

    /// Makes a file, a dangling link, a link to the file and a directory in `dir`.
    fn taken(dir: &Path) {
        fs::write(dir.join("file"), "theirs").unwrap();
        symlink(dir.join("nothing"), dir.join("dangling")).unwrap();
        symlink(dir.join("file"), dir.join("link")).unwrap();
        fs::create_dir(dir.join("dir")).unwrap();
    }

    /// Asserts that staged names taken by a file, a dangling link, a link to a file or a
    /// directory are skipped and left as they were: when all four tries collide the call
    /// fails at the staging step with `AlreadyExists` and makes nothing, and when the last
    /// try is free the file is made and the colliding entries are still there. It catches a
    /// cleanup that removes the name it failed to create, which would delete another user's
    /// entry.
    #[test]
    fn a_staged_name_that_is_taken_is_skipped_and_never_removed() {
        let owner = private("js-client-staged-taken");
        let dir = owner.path();
        taken(dir);
        let before = listing(dir);
        let mut names = from_list(&["file", "dangling", "link", "dir"]);
        let err = create_new_with(&dir.join("dest"), b"secret", 0o600, &mut names).unwrap_err();
        assert_eq!(
            (err.step(), err.kind()),
            (NewFileStep::Stage, ErrorKind::AlreadyExists)
        );
        assert_eq!(listing(dir), before);
        assert_eq!(fs::read(dir.join("file")).unwrap(), b"theirs");
        assert!(fs::symlink_metadata(dir.join("dangling")).is_ok());
        assert!(!dir.join("nothing").exists());
        assert!(dir.join("dir").is_dir());

        let mut names = from_list(&["file", "dangling", "link", "fresh"]);
        create_new_with(&dir.join("dest"), b"secret", 0o600, &mut names).unwrap();
        let mut want = before;
        want.push("dest".into());
        want.sort();
        assert_eq!(listing(dir), want);
        assert_eq!(fs::read(dir.join("dest")).unwrap(), b"secret");
        assert_eq!(fs::read(dir.join("file")).unwrap(), b"theirs");
    }

    /// Asserts that a source of names that fails is the entropy step, for a file and for a
    /// probe, and makes nothing. It catches a failure to get random bytes that is passed off
    /// as another step or ignored.
    #[test]
    fn names_that_cannot_be_made_are_the_entropy_step() {
        let owner = private("js-client-no-names");
        let dir = owner.path();
        let mut none = |_: &str| Err(io::Error::other("no entropy"));
        let err = create_new_with(&dir.join("dest"), b"x", 0o600, &mut none).unwrap_err();
        assert_eq!(err.step(), NewFileStep::Entropy);
        let err = probe_with(dir, &mut none).unwrap_err();
        assert_eq!(err.step(), NewFileStep::Entropy);
        let mut calls = 0;
        let mut second_fails = |_: &str| {
            calls += 1;
            match calls {
                1 => Ok("first".to_owned()),
                _ => Err(io::Error::other("no entropy")),
            }
        };
        let err = probe_with(dir, &mut second_fails).unwrap_err();
        assert_eq!(err.step(), NewFileStep::Entropy);
        assert!(listing(dir).is_empty());
    }

    /// Asserts that a probe name taken by a file, a dangling link, a link to a file or a
    /// directory, at the first or at the second name, is an error that leaves the entry as it
    /// was and removes only the file the probe made itself. It catches a probe that removes
    /// names it did not create.
    #[test]
    fn a_probe_name_that_is_taken_is_an_error_and_the_entry_stays() {
        let owner = private("js-client-probe-taken");
        let dir = owner.path();
        taken(dir);
        let before = listing(dir);
        for name in ["file", "dangling", "link", "dir"] {
            for (list, which) in [([name, "b"], "first"), (["a", name], "second")] {
                let err = probe_with(dir, &mut from_list(&list)).unwrap_err();
                assert_eq!(
                    (err.step(), err.kind()),
                    (NewFileStep::Probe, ErrorKind::AlreadyExists),
                    "{name} as the {which} name"
                );
                assert_eq!(listing(dir), before, "{name} as the {which} name");
            }
        }
        assert_eq!(fs::read(dir.join("file")).unwrap(), b"theirs");
        assert!(fs::symlink_metadata(dir.join("dangling")).is_ok());
        assert!(dir.join("dir").is_dir());
    }

    /// Asserts the rule for an unfit directory: another user's is, whoever can write it;
    /// root's is not; one that group or others can write is unless it is sticky; one that
    /// only the owner can write is not. It catches a rule that lets through a directory
    /// where another user could swap files, and one that refuses a sticky `/tmp`.
    #[test]
    fn who_owns_and_who_can_write_decide_if_a_directory_is_fit() {
        for (mode, owner) in [(0o755, 1000), (0o755, 0), (0o1777, 0), (0o1777, 1000)] {
            assert!(
                unfit(mode, owner, 1000).is_none(),
                "{mode:o} owned by {owner}"
            );
        }
        assert!(
            unfit(0o755, 1001, 1000)
                .unwrap()
                .contains("owned by user 1001")
        );
        assert!(unfit(0o1777, 1001, 1000).is_some());
        for mode in [0o777, 0o775, 0o757, 0o770, 0o702] {
            let why = unfit(mode, 1000, 1000).unwrap();
            assert!(why.contains("not sticky"), "{mode:o}: {why}");
        }
        assert!(unfit(0o777, 0, 0).is_some());
    }
}
