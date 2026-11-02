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
    let problems = [
        (!meta.is_file(), "is not a regular file".to_owned()),
        (
            ![0, owner].contains(&meta.uid()),
            format!("is not owned by uid {owner} or root"),
        ),
        (
            meta.mode() & 0o022 != 0,
            "is writable by group or others".to_owned(),
        ),
    ];
    if let Some((_, why)) = problems.into_iter().find(|(bad, _)| *bad) {
        return Err(refuse(why));
    }
    check_ancestors(path, owner).map_err(refuse)?;
    Ok(file)
}

/// Checks every directory above `path`: each must be a directory that root or `owner` owns
/// and that group and others cannot write. Returns the words of the first refusal, which
/// name the directory.
fn check_ancestors(path: &Path, owner: u32) -> Result<(), String> {
    for dir in path.ancestors().skip(1) {
        let meta = fs::metadata(dir).ok();
        let problems = [
            (meta.is_none(), "cannot be read"),
            (
                meta.as_ref().is_some_and(|meta| !meta.is_dir()),
                "is not a directory",
            ),
            (
                meta.as_ref()
                    .is_some_and(|meta| ![0, owner].contains(&meta.uid())),
                "is not owned by root or the owner",
            ),
            (
                meta.as_ref().is_some_and(|meta| meta.mode() & 0o022 != 0),
                "is writable by group or others",
            ),
        ];
        if let Some((_, why)) = problems.iter().find(|(bad, _)| *bad) {
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
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    /// Asserts that a file that is not root's or the owner's is refused with exactly its path
    /// and the uid, whatever the directories above it are. The integration tests cannot make
    /// a file another user owns, and changing the owner there trips the script's own check
    /// first, so the shell's check is pinned here. It catches a root process that starts a
    /// program another user owns.
    #[test]
    fn a_file_of_another_owner_is_refused_by_its_path() {
        let dir = js_testkit::dir::TestDir::under(std::env::temp_dir(), "enroll_open_owner");
        let shell = dir.path().join("shell");
        fs::write(&shell, "").unwrap();
        fs::set_permissions(&shell, fs::Permissions::from_mode(0o755)).unwrap();
        // The test runs as a user other than root, as the whole suite does: a file root
        // made would pass the check whoever the owner is.
        let other = rustix::process::geteuid().as_raw() + 1;
        let refused = open_checked(&shell, other, "hint")
            .err()
            .map(|f| (f.code(), f.text().to_owned()));
        assert_eq!(
            refused,
            Some((
                crate::exit::USAGE,
                format!("{}: is not owned by uid {other} or root", shell.display())
            ))
        );
    }

    /// Asserts that the directory walk names the first directory above the file that is not
    /// root's or the owner's, and passes when the owner is the one who made them. It is the
    /// shell's and the script's directory check on its own, without the file check in front
    /// of it. It catches a program in a directory another user can rename it out of.
    #[test]
    fn a_directory_of_another_owner_is_named() {
        let dir = js_testkit::dir::TestDir::under(std::env::temp_dir(), "enroll_walk_owner");
        let inner = dir.path().join("bin");
        fs::create_dir(&inner).unwrap();
        fs::set_permissions(&inner, fs::Permissions::from_mode(0o755)).unwrap();
        // As in the test above, the test runs as a user other than root.
        let other = rustix::process::geteuid().as_raw() + 1;
        assert_eq!(
            check_ancestors(&inner.join("shell"), other),
            Err(format!(
                "{}: is not owned by root or the owner",
                inner.display()
            ))
        );
    }
}
