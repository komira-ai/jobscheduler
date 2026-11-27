//! The host start lock over a stand-in `crun` script, with a lock file in the test's own
//! directory: when a run holds it and when it lets it go. The stand-in waits on named pipes
//! the test opens, so every step is an event and nothing waits for a clock.

use std::fs::{File, TryLockError};
use std::io::Write as _;
use std::os::fd::OwnedFd;
use std::thread::JoinHandle;

use super::*;
use crate::oci::HOST_LEASE_MARKER;
use crate::oci::crun::{Crun, RunEnd, no_scrub};
use crate::test_dir::TestDir;

/// A lease directory (with a bundle directory, and the host lease marker for a host lease)
/// and the lock its host start takes, in the test's directory.
struct Stage {
    dir: TestDir,
    lease: PathBuf,
    lock: PathBuf,
}

fn stage(name: &str, host: bool) -> Stage {
    let dir = crate::unit_dir(&format!("start-lock-{name}"));
    let lease = dir.join("lease");
    std::fs::create_dir_all(lease.join("bundle")).expect("lease");
    if host {
        std::fs::write(lease.join(HOST_LEASE_MARKER), b"").expect("the marker");
    }
    let lock = dir.join("host-start.lock");
    Stage { dir, lease, lock }
}

impl Stage {
    /// A named pipe `name` in the test's directory.
    fn pipe(&self, name: &str) -> PathBuf {
        let path = self.dir.join(name);
        rustix::fs::mknodat(
            rustix::fs::CWD,
            &path,
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::from_raw_mode(0o600),
            0,
        )
        .expect("a named pipe");
        path
    }

    /// A `crun` that writes the `--pid-file` it was given (`none` without one) to `pid-arg`
    /// in the test's directory, then runs `body`, in which `$pid` is that path; locking
    /// this stage's lock for a host lease.
    fn crun(&self, body: &str) -> Crun {
        let program = self.dir.join("crun");
        let script = format!(
            "#!/bin/sh\npid=none\nfor a in \"$@\"; do [ \"$prev\" = --pid-file ] && pid=$a; \
             prev=$a; done\necho \"$pid\" > {}\n{body}\n",
            self.dir.join("pid-arg").display()
        );
        js_testkit::spawn::install_script(&program, &script, "755");
        Crun::new(program, no_scrub, None).with_start_lock(Some(self.lock.clone()))
    }

    /// What the stand-in said its `--pid-file` was, or `None` if it never ran.
    fn pid_arg(&self) -> Option<String> {
        std::fs::read_to_string(self.dir.join("pid-arg")).ok()
    }

    /// `crun.run` over the lease, on a thread of its own with a runtime of its own.
    fn start(&self, crun: Crun) -> JoinHandle<RunEnd> {
        let (dir, lease) = (self.dir.to_path_buf(), self.lease.clone());
        std::thread::spawn(move || {
            let report = File::create(dir.join("report")).expect("the report");
            let out = OwnedFd::from(File::create(dir.join("out")).expect("stdout"));
            let err = OwnedFd::from(File::create(dir.join("err")).expect("stderr"));
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("a runtime");
            let bundle = lease.join("bundle");
            runtime.block_on(crun.run(&lease, "job-1", &bundle, report, out, err))
        })
    }

    /// The lock, opened by the test: a handle of its own, so its `flock` contends with the
    /// run's.
    fn probe(&self) -> File {
        File::options()
            .read(true)
            .open(&self.lock)
            .expect("the lock is made")
    }
}

/// Opens the named pipe `path` for writing, which waits until the stand-in has opened it for
/// reading (so it is running and at that step), and writes a line to it.
fn release(path: &Path) {
    let mut pipe = File::options()
        .write(true)
        .open(path)
        .expect("the stand-in waits");
    writeln!(pipe, "go").expect("write to the stand-in");
}

/// Opens the named pipe `path` for writing without writing to it: returns once the stand-in
/// waits on it.
fn reached(path: &Path) -> File {
    File::options()
        .write(true)
        .open(path)
        .expect("the stand-in waits")
}

/// Proves a host start holds the lock from before `crun` runs until `crun` has written its
/// pid file, and lets it go while `crun` still runs: while the stand-in waits before its
/// pid file the lock is taken, and once it has written it the test takes the lock while the
/// stand-in waits to end. `crun` was given the lease's `crun.pid`. Catches host starts that
/// do not serialize `crun`'s `/dev` step (no lock, or one taken after `crun` started) and a
/// lock kept for the whole run, which would hold every other host start on the node behind
/// a running job.
#[test]
fn a_host_start_holds_the_lock_until_crun_has_written_its_pid_file() {
    let s = stage("held", true);
    let (go, end) = (s.pipe("go"), s.pipe("end"));
    let body = format!(
        "read x < {}\necho 1 > \"$pid\"\nread y < {}",
        go.display(),
        end.display()
    );
    let running = s.start(s.crun(&body));
    let waiting = reached(&go);
    let probe = s.probe();
    assert!(
        matches!(probe.try_lock(), Err(TryLockError::WouldBlock)),
        "the lock is free while crun is at its /dev step"
    );
    writeln!(&waiting, "go").expect("write to the stand-in");
    drop(waiting);
    // Returns once the run lets the lock go: the stand-in waits on `end` until it does.
    probe.lock().expect("the lock is let go");
    probe.unlock().expect("unlock");
    release(&end);
    assert_eq!(running.join().expect("the run"), RunEnd::Exited(0));
    let pid_file = s.lease.join("crun.pid");
    assert_eq!(s.pid_arg(), Some(format!("{}\n", pid_file.display())));
    assert_eq!(std::fs::read_to_string(&pid_file).expect("written"), "1\n");
}

/// Proves a pid file an earlier start left in the lease directory does not end the hold at
/// once: it is removed before `crun` runs, and the lock stays taken while the stand-in, which
/// writes none, waits. Catches a start that takes the old file for `crun`'s and lets the lock
/// go before `crun` has passed its `/dev` step.
#[test]
fn a_pid_file_left_by_an_earlier_start_does_not_end_the_hold() {
    let s = stage("stale", true);
    std::fs::write(s.lease.join("crun.pid"), b"41\n").expect("an old pid file");
    let go = s.pipe("go");
    let running = s.start(s.crun(&format!("read x < {}\nexit 0", go.display())));
    let waiting = reached(&go);
    assert!(
        !s.lease.join("crun.pid").exists(),
        "the old pid file is left"
    );
    assert!(
        matches!(s.probe().try_lock(), Err(TryLockError::WouldBlock)),
        "the old pid file let the lock go"
    );
    writeln!(&waiting, "go").expect("write to the stand-in");
    drop(waiting);
    assert_eq!(running.join().expect("the run"), RunEnd::Exited(0));
    assert!(
        s.probe().try_lock().is_ok(),
        "the lock is kept after the run"
    );
}

/// Proves a host start whose `crun` ends without writing a pid file (it failed at its start,
/// as a `crun` that cannot make the container does) lets the lock go when it ends, and its
/// status is `crun`'s. Catches a lock that only a pid file lets go, which would hold every
/// later host start on the node for ever after one failed start.
#[test]
fn a_host_start_lets_the_lock_go_when_crun_ends_without_a_pid_file() {
    let s = stage("failed", true);
    let running = s.start(s.crun("exit 3"));
    assert_eq!(running.join().expect("the run"), RunEnd::Exited(3));
    assert!(
        s.probe().try_lock().is_ok(),
        "the lock is kept after the run"
    );
    assert!(!s.lease.join("crun.pid").exists());
}

/// Proves a contained lease's run takes no lock and gives `crun` no pid file: while its
/// stand-in runs, no lock file has been made. Catches contained starts made to wait behind
/// host starts, or each other, for a step only a host container's `crun` takes.
#[test]
fn a_contained_start_takes_no_lock_and_gives_crun_no_pid_file() {
    let s = stage("contained", false);
    let go = s.pipe("go");
    let running = s.start(s.crun(&format!("read x < {}\nexit 0", go.display())));
    let waiting = reached(&go);
    assert!(!s.lock.exists(), "a contained start made the lock");
    writeln!(&waiting, "go").expect("write to the stand-in");
    drop(waiting);
    assert_eq!(running.join().expect("the run"), RunEnd::Exited(0));
    assert_eq!(s.pid_arg(), Some("none\n".to_owned()));
}

/// Proves a host start whose lock cannot be opened (its directory is missing) fails, saying
/// which lock and why, and never starts `crun`. Catches a host start that goes on without the
/// lock, and so can still race another one on the node.
#[test]
fn a_host_start_fails_without_starting_crun_when_its_lock_cannot_be_opened() {
    let mut s = stage("no-lock", true);
    s.lock = s.dir.join("missing").join("host-start.lock");
    let running = s.start(s.crun("exit 0"));
    assert_eq!(
        running.join().expect("the run"),
        RunEnd::Failed(format!(
            "crun run: Error occurs with fs: the host start lock {}: No such file or directory \
             (os error 2)",
            s.lock.display()
        ))
    );
    assert_eq!(s.pid_arg(), None, "crun was started");
}

/// Proves a host start whose old pid file cannot be removed (a directory with an entry in it
/// stands at its path) fails, naming the path and why, lets the lock go, and never starts
/// `crun`. Catches a start that runs `crun` with a pid file already there, which would let the
/// lock go before `crun` has passed its `/dev` step.
#[test]
fn a_host_start_fails_without_starting_crun_when_the_old_pid_file_cannot_be_removed() {
    let s = stage("stuck-pid", true);
    let pid_file = s.lease.join("crun.pid");
    std::fs::create_dir_all(pid_file.join("entry")).expect("a directory at the pid file");
    let running = s.start(s.crun("exit 0"));
    assert_eq!(
        running.join().expect("the run"),
        RunEnd::Failed(format!(
            "crun run: Error occurs with fs: {}: Is a directory (os error 21)",
            pid_file.display()
        ))
    );
    assert_eq!(s.pid_arg(), None, "crun was started");
    assert!(
        s.probe().try_lock().is_ok(),
        "the lock is kept after the failed start"
    );
}

/// Proves a daemon that is not root runs a host lease's `crun` with no lock and no pid file:
/// it cannot make the node's lock, and its `crun` cannot change the host's `/dev`. Catches a
/// host start that fails for want of a lock the daemon could never have taken.
#[test]
fn a_host_start_without_a_lock_runs_crun_as_a_contained_one() {
    let s = stage("unlocked", true);
    let crun = s.crun("exit 0").with_start_lock(None);
    assert_eq!(s.start(crun).join().expect("the run"), RunEnd::Exited(0));
    assert_eq!(s.pid_arg(), Some("none\n".to_owned()));
    assert!(!s.lock.exists(), "a lock was made");
}

/// Proves the node's lock is `/dev/.js-host-start.lock` for a root daemon and that a daemon
/// that is not root has none. Catches a root daemon whose host starts lock nothing, or lock a
/// file another node's agent, or another runtime on this node, does not.
#[test]
fn the_node_lock_is_in_dev_for_a_root_daemon_alone() {
    assert_eq!(
        node_lock(0),
        Some(PathBuf::from("/dev/.js-host-start.lock"))
    );
    assert_eq!(node_lock(1000), None);
}
