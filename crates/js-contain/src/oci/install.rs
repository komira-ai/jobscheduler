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
/// install is one function whatever its callers pass.
pub(crate) type Fill<'a> = dyn FnMut(&mut File) -> io::Result<()> + Send + 'a;

/// What an installer runs on the writing thread once the program is written and before its
/// file is closed: [`nothing_while_open`] for the backend; a test forks there.
pub(crate) type WhileOpen<'a> = dyn FnMut() + Send + 'a;

/// The [`WhileOpen`] of the backend: nothing.
pub(crate) fn nothing_while_open() {}

/// The work of one install: open, write and close the staged file.
type Work<'a> = dyn FnMut() -> io::Result<()> + Send + 'a;

/// How the work of an install is run: [`on_own_table`] for the backend; a test makes it fail.
type Launch = fn(&mut Work<'_>) -> io::Result<()>;

/// Installs the program `fill` writes as `<scratch>/oci-bin/<name>` (mode 0755, replaced
/// atomically) and returns that path. `fill` is given the staged file, open for writing, on
/// a thread with a descriptor table of its own (see the module docs).
///
/// # Errors
/// The directory or the staged file could not be made, the thread or its own table could
/// not be had, `fill` failed, or the staged file could not be made executable or renamed.
/// The installed program is left as it was then. The staged file is removed, as far as it
/// can be: the error returned is the one that stopped the install, never the removal's.
///
/// # Panics
/// When `fill` panics: the panic is the caller's. The installed program is left as it was;
/// the staged file stays, and the next install of the same name by this process replaces it.
pub(crate) fn publish(scratch: &Path, name: &str, fill: &mut Fill<'_>) -> io::Result<PathBuf> {
    publish_with(on_own_table, scratch, name, fill)
}

/// [`publish`] with the way its work is run given, so a test can make it fail.
fn publish_with(
    launch: Launch,
    scratch: &Path,
    name: &str,
    fill: &mut Fill<'_>,
) -> io::Result<PathBuf> {
    let dir = scratch.join(DIR);
    std::fs::create_dir_all(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755))?;
    let staged = dir.join(format!(".{name}-{}", std::process::id()));
    // The file is closed when the closure returns, on the thread that opened it.
    let written = launch(&mut || File::create(&staged).and_then(|mut file| fill(&mut file)));
    let target = dir.join(name);
    written
        .and_then(|()| std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755)))
        .and_then(|()| std::fs::rename(&staged, &target))
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&staged);
        })
        .map(|()| target)
}

/// Runs `work` on a thread with a descriptor table of its own ([`own_table`]).
fn on_own_table(work: &mut Work<'_>) -> io::Result<()> {
    alone(own_table, work)
}

/// Runs `work` on a thread of its own that has first called `unshare`, and returns what it
/// returned; the thread has ended when this returns. A panic in `work` reaches the caller.
fn alone(unshare: fn() -> io::Result<()>, work: &mut Work<'_>) -> io::Result<()> {
    std::thread::scope(|scope| {
        let spawned = std::thread::Builder::new()
            .name("oci-install".to_owned())
            .spawn_scoped(scope, move || {
                unshare()
                    .map_err(|why| {
                        io::Error::new(why.kind(), format!("unshare(CLONE_FILES): {why}"))
                    })
                    .and_then(|()| work())
            });
        spawned.and_then(|thread| {
            thread
                .join()
                .unwrap_or_else(|payload| std::panic::resume_unwind(payload))
        })
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
#[path = "install_tests.rs"]
mod tests;
