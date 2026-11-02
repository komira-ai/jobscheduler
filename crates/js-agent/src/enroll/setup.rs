//! The host setup script the deb installs: whether it may be run, and how it is run.
//!
//! The script is run by root, so what is run is what was checked. The file is opened without
//! following a link, checked on the open descriptor (a regular file, the owner's or root's, not
//! writable by group or others), and every directory above it is checked the same way (the
//! owner's or root's, not writable by group or others). The shell is checked the same way
//! by its path (the shell itself is the system's `/usr/bin/bash`, in directories only root
//! can write). The shell is then started on `/proc/self/fd/N` of the checked descriptor, so
//! the check and the run are of one inode whatever happens to the path in between.
//!
//! Both files are opened without blocking, so a named pipe at either path is refused as not
//! a regular file instead of hanging the verb. The script's descriptor is opened without
//! close-on-exec, so the shell inherits it; nothing else is done in the child, and no
//! `unsafe` code is needed. The script runs with the caller's umask, as it does when it is
//! run by hand: it gives every file and directory it makes its mode itself (`install -m`,
//! `chmod`, `mkdir -m`), and the verb changes nothing that the process shares with other
//! threads.

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::{Command, Stdio};

use rustix::fs::{Mode, OFlags};

use super::Failure;

/// The `PATH` the script runs with. It is a root process: no directory of the caller's.
const PATH: &str = "/usr/sbin:/usr/bin:/sbin:/bin";

/// Where the script records what an apply did.
pub const MANIFEST: &str = "/var/lib/js-crun-host-setup/manifest";

/// Opens `path` for a root process to run or start: not through a link, and a regular file
/// that `owner` or root owns and group and others cannot write, in directories that root or `owner`
/// owns and group and others cannot write. `hint` follows the system's words when the file
/// cannot be opened. The returned file is the checked one.
pub(super) fn open_checked(path: &Path, owner: u32, hint: &str) -> Result<File, Failure> {
    let refuse = |why: String| Failure::Usage(format!("{}: {why}", path.display()));
    // No close-on-exec: the shell reads the script from this descriptor. No blocking: a
    // named pipe is refused below, not waited on.
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK;
    let (file, meta) = rustix::fs::open(path, flags, Mode::empty())
        .map(File::from)
        .map_err(io::Error::from)
        .and_then(|file| file.metadata().map(|meta| (file, meta)))
        .map_err(|why| refuse(format!("{why}; {hint}")))?;
    if let Some(why) = file_refusal(Found::of(&meta), owner) {
        return Err(refuse(why));
    }
    check_ancestors(path, owner).map_err(refuse)?;
    Ok(file)
}

/// What the checks need to know of a file or a directory that was found.
#[derive(Clone, Copy)]
struct Found {
    regular: bool,
    dir: bool,
    uid: u32,
    mode: u32,
}

impl Found {
    fn of(meta: &fs::Metadata) -> Self {
        Self {
            regular: meta.is_file(),
            dir: meta.is_dir(),
            uid: meta.uid(),
            mode: meta.mode(),
        }
    }
}

/// Why a program at a path may not be run by root, if it may not: it is not a regular file,
/// not root's or `owner`'s, or group or others can write it.
fn file_refusal(found: Found, owner: u32) -> Option<String> {
    let problems = [
        (!found.regular, "is not a regular file".to_owned()),
        (
            ![0, owner].contains(&found.uid),
            format!("is not owned by uid {owner} or root"),
        ),
        (
            found.mode & 0o022 != 0,
            "is writable by group or others".to_owned(),
        ),
    ];
    problems
        .into_iter()
        .find(|(bad, _)| *bad)
        .map(|(_, why)| why)
}

/// Why a directory above such a program is unfit, if it is: it cannot be read (`None`), is
/// not a directory, is not root's or `owner`'s, or group or others can write it.
fn dir_refusal(found: Option<Found>, owner: u32) -> Option<&'static str> {
    let problems = [
        (found.is_none(), "cannot be read"),
        (found.is_some_and(|f| !f.dir), "is not a directory"),
        (
            found.is_some_and(|f| ![0, owner].contains(&f.uid)),
            "is not owned by root or the owner",
        ),
        (
            found.is_some_and(|f| f.mode & 0o022 != 0),
            "is writable by group or others",
        ),
    ];
    problems
        .into_iter()
        .find(|(bad, _)| *bad)
        .map(|(_, why)| why)
}

/// Checks every directory above `path`: each must be a directory that root or `owner` owns
/// and that group and others cannot write. Returns the words of the first refusal, which
/// name the directory.
fn check_ancestors(path: &Path, owner: u32) -> Result<(), String> {
    for dir in path.ancestors().skip(1) {
        let found = fs::metadata(dir).ok().as_ref().map(Found::of);
        if let Some(why) = dir_refusal(found, owner) {
            return Err(format!("{}: {why}", dir.display()));
        }
    }
    Ok(())
}

/// A writer that does not fail: what the script prints is for a person, and a standard error
/// that is closed must not stop the script's output from being drained (a script that cannot
/// write blocks, and the setup stops half done).
struct Lenient<'a>(&'a mut dyn Write);

impl Write for Lenient<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let _ = self.0.write_all(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let _ = self.0.flush();
        Ok(())
    }
}

/// Starts `shell` on the open `script` with `flags`, with an empty environment but `PATH`
/// and `LC_ALL`, no standard input, and standard output and standard error as one pipe that
/// is copied to `err` as it fills. The child is always waited for, also when its output
/// cannot be read. Returns the exit status, `None` when a signal ended it.
fn start(
    shell: &Path,
    script: &File,
    flags: &[OsString],
    err: &mut dyn Write,
) -> io::Result<Option<i32>> {
    let (mut reader, writer) = io::pipe()?;
    let mut command = Command::new(shell);
    command
        .arg(format!("/proc/self/fd/{}", script.as_raw_fd()))
        .args(flags)
        .env_clear()
        .env("PATH", PATH)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(writer.try_clone()?)
        .stderr(writer);
    let mut child = command.spawn()?;
    // The command holds the write end; the pipe ends when the script and its children do.
    drop(command);
    let mut sink = Lenient(err);
    let copied = io::copy(&mut reader, &mut sink);
    // Never fails: a closed standard error does not stop the script.
    let _ = sink.flush();
    // A script that still writes after a failed read gets a broken pipe, not a block.
    drop(reader);
    let status = child.wait()?;
    copied?;
    Ok(status.code())
}

/// Runs the checked `script` through `shell` with `flags`. What it prints goes to `err`
/// as it prints it. Returns the word for the report: `done` (`planned` when `apply` is
/// false) for its exit 0, `nothing-to-do` for 3.
pub(super) fn run(
    shell: &Path,
    script: &File,
    flags: &[OsString],
    apply: bool,
    err: &mut dyn Write,
) -> Result<&'static str, Failure> {
    let status = start(shell, script, flags, err)
        .map_err(|why| Failure::Failed(format!("cannot run {}: {why}", shell.display())))?;
    match status {
        Some(0) => Ok(if apply { "done" } else { "planned" }),
        Some(3) => Ok("nothing-to-do"),
        Some(2) => Err(Failure::Usage(
            "the host setup refused; nothing was changed (its words are above)".to_owned(),
        )),
        other => Err(Failure::Failed(format!(
            "the host setup failed (exit status {other:?}; None is a signal, 130 and 143 an \
             interrupt); {MANIFEST} records what it did and its words are above"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A regular file or a directory with `uid` and `mode`.
    fn found(dir: bool, uid: u32, mode: u32) -> Found {
        Found {
            regular: !dir,
            dir,
            uid,
            mode,
        }
    }

    /// Asserts each answer of the check of the script and the shell, on metadata given here
    /// rather than on files the test makes, so the answers do not depend on who runs the
    /// test: root's or the owner's file that group and others cannot write is fit; a file of
    /// another user is refused naming the owner's uid, even with a safe mode; one others can
    /// write is refused even when root's; a directory in its place is refused first. It
    /// catches a root process that starts a program another user owns or can change.
    #[test]
    fn who_owns_a_program_and_who_can_write_it_decide_if_root_runs_it() {
        let owner = 1000;
        for (uid, mode) in [(0, 0o755), (owner, 0o755), (owner, 0o700), (0, 0o4755)] {
            assert_eq!(
                file_refusal(found(false, uid, mode), owner),
                None,
                "{uid} {mode:o}"
            );
        }
        let not_owner = Some("is not owned by uid 1000 or root".to_owned());
        assert_eq!(file_refusal(found(false, 1001, 0o755), owner), not_owner);
        assert_eq!(file_refusal(found(false, 1001, 0o700), owner), not_owner);
        let writable = Some("is writable by group or others".to_owned());
        for mode in [0o775, 0o757, 0o777] {
            assert_eq!(
                file_refusal(found(false, 0, mode), owner),
                writable,
                "{mode:o}"
            );
        }
        assert_eq!(
            file_refusal(found(true, 0, 0o755), owner),
            Some("is not a regular file".to_owned())
        );
    }

    /// Asserts each answer of the check of the directories above the script and the shell,
    /// on metadata given here: root's or the owner's directory that group and others cannot
    /// write is fit; one that cannot be read, a file in its place, another user's, and one
    /// others can write are each refused with their own words, in that order. It catches a
    /// program in a directory another user can rename it out of.
    #[test]
    fn who_owns_a_directory_above_a_program_decides_if_root_runs_it() {
        let owner = 1000;
        for (uid, mode) in [(0, 0o755), (owner, 0o755), (0, 0o711)] {
            assert_eq!(dir_refusal(Some(found(true, uid, mode)), owner), None);
        }
        assert_eq!(dir_refusal(None, owner), Some("cannot be read"));
        assert_eq!(
            dir_refusal(Some(found(false, 0, 0o755)), owner),
            Some("is not a directory")
        );
        assert_eq!(
            dir_refusal(Some(found(true, 1001, 0o755)), owner),
            Some("is not owned by root or the owner")
        );
        assert_eq!(
            dir_refusal(Some(found(true, 1001, 0o777)), owner),
            Some("is not owned by root or the owner")
        );
        assert_eq!(
            dir_refusal(Some(found(true, 0, 0o1777)), owner),
            Some("is writable by group or others")
        );
    }

    /// Asserts how `open_checked` puts the checks together, using modes (not owners) so the
    /// answers are the same whoever runs the test: a file others can write is refused for
    /// itself, as a usage refusal prefixed with its path, before any directory is looked at;
    /// and a fit file in a directory others can write is refused naming that directory, the
    /// nearest bad one above the file, and then the next one up once that one is fixed. (A
    /// fit file opening is shown by the integration tests, whose directories are fit all the
    /// way up.) It catches checks run in another order, a refusal that does not say which
    /// path, and a walk that names a directory further up.
    #[test]
    fn the_file_is_judged_before_the_directories_and_the_nearest_bad_one_is_named() {
        use std::os::unix::fs::PermissionsExt;
        let root = js_testkit::dir::TestDir::under(std::env::temp_dir(), "enroll_open_order");
        let mode = |path: &Path, mode| {
            fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
        };
        let (outer, inner) = (root.path().join("outer"), root.path().join("outer/inner"));
        fs::create_dir_all(&inner).unwrap();
        let program = inner.join("program");
        fs::write(&program, "").unwrap();
        let me = rustix::process::geteuid().as_raw();
        let words =
            |got: Result<File, Failure>| got.map(drop).map_err(|f| (f.code(), f.text().to_owned()));

        mode(&outer, 0o777);
        mode(&inner, 0o777);
        mode(&program, 0o777);
        assert_eq!(
            words(open_checked(&program, me, "hint")),
            Err((
                crate::exit::USAGE,
                format!("{}: is writable by group or others", program.display())
            ))
        );
        mode(&program, 0o755);
        assert_eq!(
            words(open_checked(&program, me, "hint")),
            Err((
                crate::exit::USAGE,
                format!(
                    "{}: {}: is writable by group or others",
                    program.display(),
                    inner.display()
                )
            ))
        );
        mode(&inner, 0o755);
        assert_eq!(
            words(open_checked(&program, me, "hint")),
            Err((
                crate::exit::USAGE,
                format!(
                    "{}: {}: is writable by group or others",
                    program.display(),
                    outer.display()
                )
            ))
        );
    }
}
