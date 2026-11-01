//! Waiting until a program this process has just written can be started.
//!
//! The daemon's binary and the container init are written by the backend and later
//! started by `crun`, in another process. `exec` fails with `ETXTBSY` while any process
//! holds the file open for writing, and that includes a process another thread of this
//! one forked while the file was open: the child keeps a copy of the descriptor until its
//! own exec. A rename does not help (the copy is of the open file, not of its name) and
//! the start is made by `crun`, so it cannot be retried here.
//!
//! What can be done is to wait before the file is published: [`settle`] asks the kernel
//! whether anything still has the file open for writing and checks again, a bounded number
//! of times, until nothing does. No process forked after the writer closed its descriptor
//! has a copy, so once the answer is no it stays no.

use std::io;
use std::path::Path;
use std::time::Duration;

/// How many times [`settle`] asks before it gives up.
pub(crate) const ATTEMPTS: u32 = 50;

/// How long [`settle`] waits between asks: a second in all, far longer than a fork takes
/// to exec.
pub(crate) const PAUSE: Duration = Duration::from_millis(20);

/// Returns when nothing has `path` open for writing.
///
/// # Errors
/// `ExecutableFileBusy` naming the file when something still has it open after
/// [`ATTEMPTS`] asks, or why the file could not be asked about.
pub(crate) fn settle(path: &Path) -> io::Result<()> {
    settle_with(path, ATTEMPTS, PAUSE, &mut probe)
}

/// [`settle`] with the number of asks, the wait between them and the question given.
/// `busy` says whether something has the file open for writing.
pub(crate) fn settle_with(
    path: &Path,
    attempts: u32,
    pause: Duration,
    busy: &mut dyn FnMut(&Path) -> io::Result<bool>,
) -> io::Result<()> {
    let attempts = attempts.max(1);
    for asked in 1..=attempts {
        if !busy(path)? {
            return Ok(());
        }
        if asked < attempts {
            std::thread::sleep(pause);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::ExecutableFileBusy,
        format!(
            "{}: still open for writing after {attempts} checks",
            path.display()
        ),
    ))
}

/// Moves the written `staged` file to `target` once `settle` says it can be started. When
/// it cannot, `staged` is removed and `target` (an earlier install) is left alone.
///
/// # Errors
/// What `settle` or the rename says.
pub(crate) fn publish(
    staged: &Path,
    target: &Path,
    settle: &dyn Fn(&Path) -> io::Result<()>,
) -> io::Result<()> {
    if let Err(error) = settle(staged) {
        let _ = std::fs::remove_file(staged);
        return Err(error);
    }
    std::fs::rename(staged, target)
}

/// Whether something has `path` open for writing: a read lease, which the kernel grants
/// only while no descriptor is open for writing (the count `exec` checks), is asked for
/// and dropped with the descriptor. A file system that takes no leases, or a file the
/// caller does not own, answers no: the file is published as it was before there was a
/// wait.
#[cfg(target_os = "linux")]
fn probe(path: &Path) -> io::Result<bool> {
    use std::os::fd::AsRawFd as _;
    let file = std::fs::File::open(path)?;
    // SAFETY: `fcntl` with `F_SETLEASE` takes the descriptor of `file`, open for the whole
    // call, and an integer; it reads and writes no memory.
    let granted = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLEASE, libc::F_RDLCK) };
    Ok(granted != 0 && io::Error::last_os_error().raw_os_error() == Some(libc::EAGAIN))
}

/// Whether something has `path` open for writing: not asked on this target, where no
/// container runs.
#[cfg(not(target_os = "linux"))]
fn probe(_path: &Path) -> io::Result<bool> {
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::ErrorKind;

    /// Catches a wait that gives up early or late (the number of asks is the contract),
    /// one that does not return as soon as the file is free, and an error that does not
    /// name the file.
    #[test]
    fn it_asks_again_while_busy_and_gives_up_after_the_attempts() {
        let path = Path::new("/scratch/oci-bin/.js-init-1");
        for (busy_for, attempts, asked, ok) in [
            (0, 5, 1, true),
            (3, 5, 4, true),
            (4, 5, 5, true),
            (5, 5, 5, false),
            (9, 5, 5, false),
            (9, 0, 1, false),
        ] {
            let mut asks = 0;
            let result = settle_with(path, attempts, Duration::ZERO, &mut |p| {
                assert_eq!(p, path);
                asks += 1;
                Ok(asks <= busy_for)
            });
            assert_eq!(asks, asked, "asks with {busy_for} busy of {attempts}");
            assert_eq!(result.is_ok(), ok, "{busy_for} busy of {attempts}");
            if let Err(error) = result {
                assert_eq!(error.kind(), ErrorKind::ExecutableFileBusy);
                assert!(
                    error.to_string().contains("/scratch/oci-bin/.js-init-1"),
                    "{error}"
                );
            }
        }
    }

    /// Catches an answer that is an error being taken for "free": the file could not be
    /// asked about, so it is not published.
    #[test]
    fn a_question_that_fails_is_the_error() {
        let mut asks = 0;
        let error = settle_with(Path::new("/x"), 5, Duration::ZERO, &mut |_| {
            asks += 1;
            Err(io::Error::from(ErrorKind::PermissionDenied))
        })
        .expect_err("fails");
        assert_eq!((asks, error.kind()), (1, ErrorKind::PermissionDenied));
    }

    /// Catches a question that cannot see a descriptor open for writing, the descriptor a
    /// forked child holds being one: the kernel's count of writers is what `exec` checks.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_kernel_is_asked_whether_the_file_is_open_for_writing() {
        let dir = std::env::current_exe()
            .expect("test binary")
            .parent()
            .expect("deps directory")
            .join("js-contain-unit")
            .join(format!("install-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("program");
        let writer = std::fs::File::create(&path).expect("create");
        assert!(probe(&path).expect("open file"), "open for writing");
        let error = settle_with(&path, 3, Duration::ZERO, &mut probe).expect_err("busy");
        assert_eq!(error.kind(), ErrorKind::ExecutableFileBusy);
        drop(writer);
        assert!(!probe(&path).expect("closed file"), "closed");
        settle_with(&path, 3, Duration::ZERO, &mut probe).expect("free");
        // A file the kernel does not take a lease on is not in the way.
        assert!(!probe(Path::new("/proc/self/status")).expect("proc file"));
        assert_eq!(
            probe(&dir.join("absent")).expect_err("no file").kind(),
            ErrorKind::NotFound
        );
    }
}
