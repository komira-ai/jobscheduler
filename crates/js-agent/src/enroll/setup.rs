//! The host setup script the deb installs: whether it may be run, and how it is run.
//!
//! The script is run by root, so what is run is what was checked. The file is opened without
//! following a link, checked on the open descriptor (a regular file, the owner's or root's, not
//! writable by group or others), and every directory above it is checked the same way (the
//! owner's or root's, not writable by group or others). The shell is checked the same way
//! by its path (the shell itself is the system's `/usr/bin/bash`, in directories only root
//! can write). The shell is then started on `/proc/self/fd/N` of the checked descriptor, so
//! the check and the run are of one inode whatever happens to the path in between.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, BorrowedFd, RawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

use rustix::fs::{Mode, OFlags};
use rustix::io::{FdFlags, fcntl_setfd};

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
    let (file, meta) = OpenOptions::new()
        .read(true)
        .custom_flags(OFlags::NOFOLLOW.bits().cast_signed())
        .open(path)
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
            return Err(refuse(format!("{}: {why}", dir.display())));
        }
    }
    Ok(file)
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

/// What the forked child does before it execs the shell: umask 022 whatever the caller's was
/// (the script sets its own where it needs more), and the checked descriptor is kept open
/// across the exec so the shell can read the script from `/proc/self/fd/N`. Two system
/// calls and nothing else.
fn before_exec(fd: RawFd) -> io::Result<()> {
    rustix::process::umask(Mode::from_bits_truncate(0o022));
    // SAFETY: `fd` is the descriptor of the checked script, open in this process for the
    // whole of the call.
    let script = unsafe { BorrowedFd::borrow_raw(fd) };
    fcntl_setfd(script, FdFlags::empty()).map_err(Into::into)
}

/// The function run in the forked child.
fn in_child(fd: RawFd) -> impl FnMut() -> io::Result<()> + Send + Sync + 'static {
    move || before_exec(fd)
}

/// Starts `shell` on the open `script` with `flags`, with an empty environment but `PATH`
/// and `LC_ALL`, no standard input, and standard output and standard error as one pipe that
/// is copied to `err` as it fills. Returns the exit status, `None` when a signal ended it.
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
    // SAFETY: `before_exec` makes two system calls and nothing else (no allocation, no
    // lock) in the forked child before it execs.
    unsafe {
        command.pre_exec(in_child(script.as_raw_fd()));
    }
    let mut child = command.spawn()?;
    // The command holds the write end; the pipe ends when the script and its children do.
    drop(command);
    let mut sink = Lenient(err);
    io::copy(&mut reader, &mut sink)?;
    sink.flush()?;
    Ok(child.wait()?.code())
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

    /// Asserts that the function run in the child clears the close-on-exec flag of the
    /// descriptor it is given (so the shell can read the script from it) and leaves the
    /// umask 022. It catches a script descriptor that does not survive the exec, which the
    /// integration tests only see as a failed run.
    #[test]
    fn the_child_keeps_the_script_open_and_sets_the_umask() {
        let file = File::open("/dev/null").unwrap();
        in_child(file.as_raw_fd())().unwrap();
        let flags = rustix::io::fcntl_getfd(&file).unwrap();
        assert!(!flags.contains(FdFlags::CLOEXEC));
        let before = rustix::process::umask(Mode::from_bits_truncate(0o022));
        assert_eq!(before.bits(), 0o022);
    }
}
