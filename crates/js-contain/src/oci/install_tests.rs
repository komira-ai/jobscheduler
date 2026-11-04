//! The tests of [`super`]: a program installed while another thread forks still starts, and
//! every way an install can fail leaves the installed program as it was.

use std::io::Write as _;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;

use js_testkit::forked::ForkedChild;

use super::*;
use crate::oci::{helper, init};

/// The names in `dir`, sorted.
fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<_> = std::fs::read_dir(dir)
        .expect("dir")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// The name an install by this process stages `name` under.
fn staged(name: &str) -> String {
    format!(".{name}-{}", std::process::id())
}

/// A writer of `bytes` that counts its calls in `writes`: one function for every test, so a
/// test in which it must not run still has it run by the others.
fn writer<'a>(
    bytes: &'static [u8],
    writes: &'a AtomicU32,
) -> impl FnMut(&mut File) -> io::Result<()> + Send + 'a {
    move |file| {
        writes.fetch_add(1, Ordering::SeqCst);
        file.write_all(bytes)
    }
}

/// `scratch` with the program `p` installed holding `one`, written once.
fn installed_one(scratch: &Path) -> PathBuf {
    let writes = AtomicU32::new(0);
    let first = publish(scratch, "p", &mut writer(b"one", &writes)).expect("first");
    assert_eq!(writes.load(Ordering::SeqCst), 1);
    first
}

/// The defect: another thread of the process forks while an installer's staged file is open
/// for writing (tokio's, a library's, another `Command::spawn`), and its child holds a copy of
/// the write descriptor until its own exec, so `crun`'s exec of the installed program is
/// refused with `ETXTBSY` ("Text file busy"). Both installers of the backend, the init's and
/// the daemon's, are run through their writer-open seam, and each installed program is
/// started while the forked child is still held.
///
/// The held child has a copy of every descriptor of its process, so the scenario runs in a
/// process of its own ([`forked_child_scenario`]) and no other test of this binary depends
/// on when it is let go.
#[test]
fn install_with_a_forked_child_holding_the_write_fd_does_not_fail_the_start() {
    let exe = std::env::current_exe().expect("the test binary");
    let ran = Command::new(exe)
        .args([
            "--exact",
            "oci::install::tests::forked_child_scenario",
            "--ignored",
        ])
        .stdin(Stdio::null())
        .output()
        .expect("the child ran");
    let out = String::from_utf8_lossy(&ran.stdout);
    assert!(ran.status.success(), "{ran:?}");
    assert!(
        out.contains("test oci::install::tests::forked_child_scenario ... ok"),
        "the scenario did not run: {out}"
    );
}

/// The scenario of [`install_with_a_forked_child_holding_the_write_fd_does_not_fail_the_start`],
/// run by it as a process of its own: the init is installed and started while a forked child
/// is held, then the daemon (a copy of that init, which runs anywhere) the same way.
#[test]
#[ignore = "a process of its own of install_with_a_forked_child_holding_the_write_fd_does_not_fail_the_start"]
fn forked_child_scenario() {
    let scratch = crate::unit_dir("install-forked");
    let init = started_while_a_forked_child_is_held(&mut |while_open| {
        init::install_with(&scratch, while_open)
    });
    assert_eq!(init, scratch.join("oci-bin/js-init"));
    let daemon = started_while_a_forked_child_is_held(&mut |while_open| {
        helper::install_daemon_with(&scratch, &init, while_open)
    });
    assert_eq!(daemon, scratch.join("oci-bin/daemon"));
}

/// Runs `install`, whose writer-open seam forks a child from another thread of this process
/// and holds it between fork and exec; then starts the installed program while the child is
/// still held, and returns its path. The program is the init: run with no arguments, it
/// prints its usage and exits 2.
fn started_while_a_forked_child_is_held(
    install: &mut dyn FnMut(&mut WhileOpen<'_>) -> io::Result<PathBuf>,
) -> PathBuf {
    let (open, opened) = mpsc::channel::<()>();
    let (forked, fork_done) = mpsc::channel::<()>();
    let forker = std::thread::spawn(move || {
        opened.recv().expect("the staged file is open");
        let child = ForkedChild::before_exec();
        forked.send(()).expect("the writer waits");
        child
    });
    let installed = install(&mut move || {
        open.send(()).expect("the forker waits");
        fork_done.recv().expect("the forker forked");
    })
    .expect("installed");
    let child = forker.join().expect("the forker");
    let started = Command::new(&installed)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    child.finish();
    let status = started.expect("the installed program starts, not busy");
    assert_eq!(status.code(), Some(2), "the init's usage exit");
    installed
}

/// Catches a failed write that still replaces the installed program (a container would
/// start a cut-short file) or leaves its staged file behind.
#[test]
fn a_write_that_fails_keeps_the_installed_program_and_no_staged_file() {
    let scratch = crate::unit_dir("install-fails");
    let first = installed_one(&scratch);
    let failed = publish(&scratch, "p", &mut |f| {
        f.write_all(b"tw")?;
        Err(io::Error::other("the source ended"))
    });
    let why = failed.expect_err("the write failed");
    assert_eq!(why.to_string(), "the source ended");
    assert_eq!(std::fs::read(&first).expect("installed"), b"one");
    assert_eq!(names(first.parent().expect("dir")), ["p"]);
}

/// Catches a write that goes on, on the process's shared table, when the thread could not
/// have one of its own (the race would be back, unseen): the install fails, naming the call,
/// the writer never runs, and the installed program stays.
#[test]
fn a_refused_own_table_fails_the_install_and_writes_nothing() {
    fn refused() -> io::Result<()> {
        Err(io::Error::from_raw_os_error(1))
    }
    let scratch = crate::unit_dir("install-unshare");
    let first = installed_one(&scratch);
    let writes = AtomicU32::new(0);
    let launch: Launch = |work| alone(refused, work);
    let failed = publish_with(launch, &scratch, "p", &mut writer(b"two", &writes));
    let why = failed.expect_err("no table of its own");
    assert_eq!(
        writes.load(Ordering::SeqCst),
        0,
        "written without its own table"
    );
    assert_eq!(why.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(
        why.to_string(),
        "unshare(CLONE_FILES): Operation not permitted (os error 1)"
    );
    assert_eq!(std::fs::read(&first).expect("installed"), b"one");
    assert_eq!(names(first.parent().expect("dir")), ["p"]);
}

/// Catches a thread that could not be made taken for a finished write, or the work done on
/// the caller's thread instead: the launch's error is returned as it came, the writer never
/// runs, and the installed program stays.
#[test]
fn a_thread_that_cannot_be_made_fails_the_install_and_writes_nothing() {
    let scratch = crate::unit_dir("install-no-thread");
    let first = installed_one(&scratch);
    let writes = AtomicU32::new(0);
    let launch: Launch = |_| Err(io::Error::new(io::ErrorKind::WouldBlock, "no thread"));
    let failed = publish_with(launch, &scratch, "p", &mut writer(b"two", &writes));
    let why = failed.expect_err("no thread");
    assert_eq!(
        writes.load(Ordering::SeqCst),
        0,
        "written on the caller's thread"
    );
    assert_eq!(why.kind(), io::ErrorKind::WouldBlock);
    assert_eq!(why.to_string(), "no thread");
    assert_eq!(std::fs::read(&first).expect("installed"), b"one");
    assert_eq!(names(first.parent().expect("dir")), ["p"]);
}

/// Catches an install that writes anywhere when its directory cannot be made (the scratch
/// directory is a file): it fails with that error, before the writer runs.
#[test]
fn a_directory_that_cannot_be_made_fails_before_the_writer_runs() {
    let scratch = crate::unit_dir("install-no-dir");
    let file = scratch.join("a-file");
    std::fs::write(&file, b"").expect("a file");
    let writes = AtomicU32::new(0);
    let failed = publish(&file, "p", &mut writer(b"one", &writes));
    assert_eq!(
        failed.expect_err("no dir").kind(),
        io::ErrorKind::NotADirectory
    );
    assert_eq!(writes.load(Ordering::SeqCst), 0);
    assert_eq!(names(&scratch), ["a-file"]);
}

/// Catches a staged file that cannot be made (its name is taken by a directory) passed over,
/// or the error of the clean-up returned in place of the one that stopped the install: the
/// install fails with the create's error, the writer never runs, the installed program
/// stays, and the clean-up, which cannot remove a directory, leaves it.
#[test]
fn a_staged_file_that_cannot_be_made_fails_with_its_own_error() {
    let scratch = crate::unit_dir("install-staged-taken");
    let first = installed_one(&scratch);
    let dir = first.parent().expect("dir");
    std::fs::create_dir(dir.join(staged("p"))).expect("the staged name taken");
    let writes = AtomicU32::new(0);
    let failed = publish(&scratch, "p", &mut writer(b"two", &writes));
    assert_eq!(
        failed.expect_err("taken").kind(),
        io::ErrorKind::IsADirectory
    );
    assert_eq!(writes.load(Ordering::SeqCst), 0);
    assert_eq!(std::fs::read(&first).expect("installed"), b"one");
    assert_eq!(names(dir), [staged("p"), "p".to_owned()]);
}

/// Catches a staged file that cannot be made executable (it is gone: the writer removed it)
/// renamed into place anyway: the install fails with the chmod's error after the write, and
/// the installed program keeps its bytes.
#[test]
fn a_staged_file_that_cannot_be_made_executable_fails_and_keeps_the_program() {
    let scratch = crate::unit_dir("install-no-chmod");
    let first = installed_one(&scratch);
    let dir = first.parent().expect("dir").to_owned();
    let gone = dir.join(staged("p"));
    let failed = publish(&scratch, "p", &mut |f| {
        f.write_all(b"two")?;
        std::fs::remove_file(gone.as_path())
    });
    assert_eq!(failed.expect_err("gone").kind(), io::ErrorKind::NotFound);
    assert_eq!(std::fs::read(first).expect("installed"), b"one");
    assert_eq!(names(&dir), ["p"]);
}

/// Catches a first install whose rename fails (the name is taken by a directory) leaving its
/// staged file behind: the install fails with the rename's error, after the write, and the
/// directory has nothing new in it.
#[test]
fn a_first_install_that_cannot_be_renamed_leaves_no_staged_file() {
    let scratch = crate::unit_dir("install-target-taken");
    let dir = scratch.join("oci-bin");
    std::fs::create_dir_all(dir.join("p/inside")).expect("the name taken");
    let writes = AtomicU32::new(0);
    let failed = publish(&scratch, "p", &mut writer(b"one", &writes));
    assert_eq!(
        failed.expect_err("taken").kind(),
        io::ErrorKind::IsADirectory
    );
    assert_eq!(writes.load(Ordering::SeqCst), 1);
    assert_eq!(names(&dir), ["p"]);
    assert_eq!(names(&dir.join("p")), ["inside"]);
}

/// Catches a daemon that cannot be read installed as an empty program, or a staged file left
/// by it: the first install fails with the open's error and installs nothing.
#[test]
fn a_daemon_that_cannot_be_read_installs_nothing() {
    let scratch = crate::unit_dir("install-no-daemon");
    let missing = scratch.join("no-such-daemon");
    let failed = helper::install_daemon(&scratch, &missing);
    assert_eq!(
        failed.expect_err("no daemon").kind(),
        io::ErrorKind::NotFound
    );
    assert!(names(&scratch.join("oci-bin")).is_empty());
}

/// Catches a panic while writing taken for a finished write, so that a cut-short program is
/// installed: the panic reaches the caller with its own payload, the installed program keeps
/// its bytes, the staged file stays as `publish` says, and the next install replaces both.
#[test]
fn a_panic_while_writing_keeps_the_installed_program_and_the_next_install_succeeds() {
    let scratch = crate::unit_dir("install-panics");
    let first = installed_one(&scratch);
    let panicked = std::panic::catch_unwind(|| {
        publish(&scratch, "p", &mut |f| {
            f.write_all(b"tw")?;
            panic!("the writer panicked")
        })
    });
    let payload = panicked.expect_err("the panic reached the caller");
    assert_eq!(payload.downcast_ref::<&str>(), Some(&"the writer panicked"));
    assert_eq!(std::fs::read(&first).expect("installed"), b"one");
    let dir = first.parent().expect("dir");
    assert_eq!(names(dir), [staged("p"), "p".to_owned()]);
    let writes = AtomicU32::new(0);
    let next = publish(&scratch, "p", &mut writer(b"three", &writes)).expect("next");
    assert_eq!(std::fs::read(next).expect("installed"), b"three");
    assert_eq!(names(dir), ["p"]);
}
