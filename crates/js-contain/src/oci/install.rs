//! Putting a program on the node for `crun` to execute: the daemon's own binary
//! ([`super::helper::install_daemon`]) and the container's init ([`super::init::install`]),
//! each written to a staged name in `<scratch>/oci-bin`, made `0755` and renamed into place.

use std::fs::File;
use std::io;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

/// The directory of the scratch directory the programs are installed in.
const DIR: &str = "oci-bin";

/// Installs the program `fill` writes as `<scratch>/oci-bin/<name>` (mode 0755, replaced
/// atomically) and returns that path. `fill` is given the staged file, open for writing.
///
/// # Errors
/// The directory or the staged file could not be made, `fill` failed, or the staged file
/// could not be made executable or renamed. The installed program is left as it was then,
/// and the staged file is removed.
pub(crate) fn publish(
    scratch: &Path,
    name: &str,
    fill: impl FnOnce(&mut File) -> io::Result<()> + Send,
) -> io::Result<PathBuf> {
    let dir = scratch.join(DIR);
    std::fs::create_dir_all(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755))?;
    let staged = dir.join(format!(".{name}-{}", std::process::id()));
    let written = File::create(&staged).and_then(|mut file| fill(&mut file));
    let target = dir.join(name);
    written
        .and_then(|()| std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755)))
        .and_then(|()| std::fs::rename(&staged, &target))
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&staged);
        })
        .map(|()| target)
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
        let installed = publish(&scratch, "program", |file| {
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
        let first = publish(&scratch, "p", |f| f.write_all(b"one")).expect("first");
        let failed = publish(&scratch, "p", |f| {
            f.write_all(b"tw")?;
            Err(io::Error::other("the source ended"))
        });
        let why = failed.expect_err("the write failed");
        assert_eq!(why.to_string(), "the source ended");
        assert_eq!(std::fs::read(&first).expect("installed"), b"one");
        assert_eq!(names(first.parent().expect("dir")), ["p"]);
    }

    /// Catches a panic while writing taken for a finished write (the program would be
    /// installed cut short) instead of reaching the caller.
    #[test]
    #[should_panic(expected = "the writer panicked")]
    fn a_panic_while_writing_reaches_the_caller() {
        let scratch = crate::unit_dir("install-panics");
        let _ = publish(&scratch, "p", |_| panic!("the writer panicked"));
    }
}
