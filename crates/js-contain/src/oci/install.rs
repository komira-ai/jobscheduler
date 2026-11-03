//! Putting a program on the node for `crun` to execute: the daemon's own binary
//! ([`super::helper::install_daemon`]) and the container's init ([`super::init::install`]),
//! each written to a staged name in `<scratch>/oci-bin`, made `0755` and renamed into place.
//!
//! **The write runs on a thread with a descriptor table of its own.** The kernel refuses to
//! execute a file that anything has open for writing (`ETXTBSY`, "Text file busy"). The
//! staged file is closed before the rename, but a `fork` by any other thread of the process
//! while it is open (tokio's, a library's, another `Command::spawn`) gives the child a copy
//! of the write descriptor, which the child keeps until its own `exec`: close-on-exec closes
//! it only then. If `crun` executes the installed program inside that window, the container
//! does not start. The rename does not help: the copy is of the open file, not of its name.
//!
//! A forked child gets a copy of the descriptor table of the thread that forked. So the
//! staged file is opened, written and closed on a thread made for it that has first left the
//! process's shared table (`unshare(CLONE_FILES)`, [`own_table`]): the write descriptor is
//! only ever in that thread's own table, which no other thread forks from, and that thread
//! forks nothing. Once it has closed the file, nothing holds it open for writing, whatever
//! the rest of the process does. When the thread cannot have a table of its own, the install
//! fails, saying so, and writes nothing.
//!
//! The other fixes considered, and why not them:
//! - Installing before any other thread of the process exists. The agent's signal thread
//!   starts before its flags, and so the store the programs go in, are read; every caller of
//!   [`super::Oci::new`] (the agent, its image pull verb, the tests, which run on threads of
//!   one process) would have to keep that order, and nothing here could check it.
//! - Retrying a start that `crun` reports failed on the exec of the daemon or the init.
//!   `crun` executes them inside the container, so the retry is of a whole `crun run` (and of
//!   each helper run, blocking or not), told apart by the words of `crun`'s log; and a
//!   bounded retry still fails a start when the window outlasts it.
//! - Writing through a child process that has ended before the rename: a process start per
//!   install, and a program to write with (a shell, or a new mode of the daemon's binary) that
//!   the backend would depend on.
//! - Waiting before the rename until the kernel would grant a read lease on the staged file
//!   (`F_SETLEASE`, granted only while nothing has it open for writing): an open for writing
//!   while the lease is held sends `SIGIO`, which ends the process by default.

use std::fs::File;
use std::io;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

/// The directory of the scratch directory the programs are installed in.
const DIR: &str = "oci-bin";

/// What writes a program into its staged file. A trait object, not a type parameter, so the
/// install is one function whatever its callers pass (each instantiation of a generic one would
/// be measured on its own).
pub(crate) type Fill<'a> = dyn FnMut(&mut File) -> io::Result<()> + Send + 'a;

/// Installs the program `fill` writes as `<scratch>/oci-bin/<name>` (mode 0755, replaced
/// atomically) and returns that path. `fill` is given the staged file, open for writing, on
/// a thread with a descriptor table of its own (see the module docs).
///
/// # Errors
/// The directory or the staged file could not be made, the thread or its own table could
/// not be had, `fill` failed, or the staged file could not be made executable or renamed.
/// The installed program is left as it was then, and the staged file is removed.
///
/// # Panics
/// When `fill` panics: the panic is the caller's.
pub(crate) fn publish(scratch: &Path, name: &str, fill: &mut Fill<'_>) -> io::Result<PathBuf> {
    publish_with(own_table, scratch, name, fill)
}

/// [`publish`] with the call that gives the writing thread its own table given, so a test
/// can make it fail.
fn publish_with(
    unshare: fn() -> io::Result<()>,
    scratch: &Path,
    name: &str,
    fill: &mut Fill<'_>,
) -> io::Result<PathBuf> {
    let dir = scratch.join(DIR);
    std::fs::create_dir_all(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755))?;
    let staged = dir.join(format!(".{name}-{}", std::process::id()));
    // The file is closed when the closure returns, on the thread that opened it.
    let written = alone(unshare, &mut || {
        File::create(&staged).and_then(|mut file| fill(&mut file))
    });
    let target = dir.join(name);
    written
        .and_then(|()| std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755)))
        .and_then(|()| std::fs::rename(&staged, &target))
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&staged);
        })
        .map(|()| target)
}

/// Runs `work` on a thread of its own that has first called `unshare`, and returns what it
/// returned; the thread has ended when this returns. A panic in `work` reaches the caller.
fn alone(
    unshare: fn() -> io::Result<()>,
    work: &mut (dyn FnMut() -> io::Result<()> + Send),
) -> io::Result<()> {
    std::thread::scope(|scope| {
        let thread = std::thread::Builder::new()
            .name("oci-install".to_owned())
            .spawn_scoped(scope, move || {
                unshare()
                    .map_err(|why| {
                        io::Error::new(why.kind(), format!("unshare(CLONE_FILES): {why}"))
                    })
                    .and_then(|()| work())
            })?;
        thread
            .join()
            .unwrap_or_else(|payload| std::panic::resume_unwind(payload))
    })
}

/// Gives the calling thread a descriptor table of its own: a copy of the process's, which
/// the other threads no longer share with it.
fn own_table() -> io::Result<()> {
    // SAFETY: the one caller is the thread `alone` makes for one install, before it does
    // anything else. That thread uses no descriptor another thread made (it opens the
    // staged file, and the daemon it copies from, itself) and hands none out (it closes
    // both before it ends), so no thread ever uses a descriptor across the two tables.
    unsafe { rustix::thread::unshare_unsafe(rustix::thread::UnshareFlags::FILES) }
        .map_err(io::Error::from)
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::process::{Command, Stdio};
    use std::sync::mpsc;

    use js_testkit::forked::ForkedChild;

    use super::*;

    /// The names in `dir`, sorted.
    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .expect("dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// The defect: another thread of the process forks while the staged file is open for
    /// writing (tokio's, a library's, another `Command::spawn`), and its child holds a copy
    /// of the write descriptor until its own exec, so `crun`'s exec of the installed program
    /// is refused with `ETXTBSY` ("Text file busy"). [`ForkedChild`] holds that child
    /// between fork and exec for as long as the test needs, so the interleaving is the only
    /// one: the fork happens on a thread of the process while `fill` has the file open, and
    /// the installed program is started while the child is still held.
    #[test]
    fn install_with_a_forked_child_holding_the_write_fd_does_not_fail_the_start() {
        let scratch = crate::unit_dir("install-forked");
        let (open, opened) = mpsc::channel::<()>();
        let (forked, fork_done) = mpsc::channel::<()>();
        let forker = std::thread::spawn(move || {
            opened.recv().expect("the staged file is open");
            let child = ForkedChild::before_exec();
            forked.send(()).expect("the writer waits");
            child
        });
        let installed = publish(&scratch, "program", &mut move |file| {
            file.write_all(crate::oci::init::BINARY)?;
            open.send(()).expect("the forker waits");
            fork_done.recv().expect("the forker forked");
            Ok(())
        })
        .expect("installed");
        let child = forker.join().expect("the forker");
        // The init, run with no arguments, prints its usage and exits: only the start
        // matters here.
        let started = Command::new(&installed)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        child.finish();
        let status = started.unwrap_or_else(|e| panic!("start {}: {e}", installed.display()));
        assert_eq!(status.code(), Some(2), "the init's usage exit");
    }

    /// Catches a failed write that still replaces the installed program (a container would
    /// start a cut-short file) or leaves its staged file behind.
    #[test]
    fn a_write_that_fails_keeps_the_installed_program_and_no_staged_file() {
        let scratch = crate::unit_dir("install-fails");
        let first = publish(&scratch, "p", &mut |f| f.write_all(b"one")).expect("first");
        let failed = publish(&scratch, "p", &mut |f| {
            f.write_all(b"tw")?;
            Err(io::Error::other("the source ended"))
        });
        let why = failed.expect_err("the write failed");
        assert_eq!(why.to_string(), "the source ended");
        assert_eq!(std::fs::read(&first).expect("installed"), b"one");
        assert_eq!(names(first.parent().expect("dir")), ["p"]);
    }

    /// Catches a write that goes on, on the process's shared table, when the thread could
    /// not have one of its own (the race would be back, unseen): the install fails, naming
    /// the call, nothing is written, and the installed program stays.
    #[test]
    fn a_refused_own_table_fails_the_install_and_writes_nothing() {
        let scratch = crate::unit_dir("install-unshare");
        let first = publish(&scratch, "p", &mut |f| f.write_all(b"one")).expect("first");
        let refused: fn() -> io::Result<()> = || Err(io::Error::from_raw_os_error(1));
        let failed = publish_with(refused, &scratch, "p", &mut |f| f.write_all(b"two"));
        let why = failed.expect_err("no table of its own");
        assert_eq!(why.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(
            why.to_string(),
            "unshare(CLONE_FILES): Operation not permitted (os error 1)"
        );
        assert_eq!(std::fs::read(&first).expect("installed"), b"one");
        assert_eq!(names(first.parent().expect("dir")), ["p"]);
    }

    /// Catches a panic while writing taken for a finished write (the program would be
    /// installed cut short) instead of reaching the caller.
    #[test]
    #[should_panic(expected = "the writer panicked")]
    fn a_panic_while_writing_reaches_the_caller() {
        let scratch = crate::unit_dir("install-panics");
        let _ = publish(&scratch, "p", &mut |_| panic!("the writer panicked"));
    }
}
