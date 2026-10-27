//! The keeper: a process for each lease that kills the lease cgroup when the daemon dies.
//!
//! A daemon killed by SIGKILL, the kernel's OOM killer or a panic that aborts cannot
//! end its containers: nothing kills a `crun run` or the container it started when the
//! daemon dies, so they keep running. A start-up sweep removes them only when a daemon
//! is started again on the node. Until then the job would run, for as long as the work
//! takes, beside the copy the scheduler starts elsewhere once the handover grace has
//! passed. So each lease gets a keeper once its cgroup is made: a process whose stdin is
//! a pipe only the daemon holds the other end of. The daemon never writes it; when the
//! daemon dies the kernel closes that end, the keeper reads end-of-file and writes the
//! lease cgroup's `cgroup.kill`, which kills every process in the lease, the container's
//! among them. It writes it again every second, [`KILL_TRIES`] times in all: a `crun
//! run` the dead daemon left in flight can still put the container's first process in
//! the lease after the first write. It stops early once the cgroup is gone (the next
//! start's sweep removed it). The container, the lease cgroup and the scratch directory
//! stay for that sweep; nothing in them runs.
//!
//! The keeper process itself is the embedding binary's own mode, started by a
//! [`KeeperLauncher`]; this module holds what is the same for every launcher: the handle
//! that owns the process. It must ignore SIGHUP, SIGINT and SIGTERM, so a service manager
//! that stops the daemon's unit with SIGTERM does not stop it before it has read
//! end-of-file. The daemon removes the keeper (SIGKILL, then reaps it) once the lease is
//! cleaned.

use std::io;
use std::path::Path;
use std::process::Child;

/// How many times, a second apart, the keeper writes `cgroup.kill`.
pub const KILL_TRIES: u32 = 30;

/// How a keeper process is started: by the embedding binary, which owns the code the
/// process runs.
pub trait KeeperLauncher {
    /// Starts a keeper that, once the end of its stdin that this process holds is closed,
    /// writes `1` to the file `kill` up to `tries` times a second apart, and returns once
    /// the process is running as a keeper (it has answered that it is). Its stdin must be
    /// a pipe, it must be in a process group of its own, and it must ignore SIGHUP,
    /// SIGINT and SIGTERM.
    ///
    /// # Errors
    /// The process could not be started, or did not answer as a keeper.
    fn launch(&self, kill: &Path, tries: u32) -> io::Result<Child>;
}

/// One lease's keeper. Dropped, it is SIGKILLed and reaped without writing anything:
/// the pipe it reads closes only after it is dead.
#[derive(Debug)]
pub struct Keeper {
    child: Child,
}

impl Keeper {
    /// Starts the keeper of the cgroup whose `cgroup.kill` is `kill`, with `launcher`
    /// ([`KILL_TRIES`] tries), and returns once it runs as a keeper.
    ///
    /// # Errors
    /// The keeper could not be started, or its program did not answer as a keeper.
    pub fn watch(launcher: &impl KeeperLauncher, kill: &Path) -> io::Result<Self> {
        let child = launcher.launch(kill, KILL_TRIES).map_err(cannot_start)?;
        Ok(Self { child })
    }

    /// What a daemon's death does: closes the daemon's end of the keeper's stdin.
    /// The keeper stays this process's child, reaped on drop.
    #[cfg(test)]
    pub fn orphan(&mut self) {
        drop(self.child.stdin.take());
    }
}

/// `e`, saying that the lease's keeper could not be started.
fn cannot_start(e: io::Error) -> io::Error {
    io::Error::new(e.kind(), format!("start the lease's keeper: {e}"))
}

impl Drop for Keeper {
    fn drop(&mut self) {
        // SIGKILL to the keeper alone (it is this process's unreaped child, so the pid
        // is still its own), then reap it. Its stdin closes after, with the struct.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::process::{Command, Stdio};

    /// A launcher whose keeper is `cat`: it holds its stdin pipe and ends when the pipe
    /// closes, as a keeper does. It records what it was asked.
    struct Cat {
        program: &'static str,
        asked: RefCell<Vec<(std::path::PathBuf, u32)>>,
    }

    impl Cat {
        fn new() -> Self {
            Self {
                program: "cat",
                asked: RefCell::default(),
            }
        }
    }

    impl KeeperLauncher for Cat {
        fn launch(&self, kill: &Path, tries: u32) -> io::Result<Child> {
            self.asked.borrow_mut().push((kill.to_owned(), tries));
            Command::new(self.program)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .spawn()
        }
    }

    /// Catches a keeper that is not handed the cgroup it is to kill or the number of
    /// tries, a handle that stops its process while the daemon still holds the pipe (the
    /// keeper must outlive the lease's running), and one that does not end it once the
    /// pipe closes (the stand-in ends at end-of-file, as the keeper does before it writes; the
    /// test waits for that end, so a keeper that went on would hang it).
    #[test]
    fn a_keeper_runs_while_held_and_ends_when_its_pipe_closes() {
        let launcher = Cat::new();
        let mut keeper = Keeper::watch(&launcher, Path::new("/k/cgroup.kill")).expect("keeper");
        assert_eq!(
            *launcher.asked.borrow(),
            [(Path::new("/k/cgroup.kill").to_owned(), KILL_TRIES)]
        );
        assert!(
            keeper.child.try_wait().expect("wait").is_none(),
            "the keeper exited while held"
        );
        keeper.orphan();
        // Returns only when the process has ended: a keeper that went on would hang here.
        keeper
            .child
            .wait()
            .expect("the keeper ends once its pipe closes");
    }

    /// Catches a keeper that cannot be started taken for one that runs, and an error
    /// that does not say what failed.
    #[test]
    fn a_keeper_that_cannot_start_is_an_error() {
        let launcher = Cat {
            program: "/nonexistent-keeper/program",
            asked: RefCell::default(),
        };
        let error = Keeper::watch(&launcher, Path::new("/k")).expect_err("no program");
        assert_eq!(error.kind(), io::ErrorKind::NotFound, "{error}");
        assert!(
            error.to_string().starts_with("start the lease's keeper: "),
            "{error}"
        );
    }

    /// Catches a keeper whose drop (the daemon removing it once the lease is cleaned)
    /// leaves the process behind: dropped, it is killed and reaped before the drop
    /// returns, so its pid is gone.
    #[test]
    fn a_dropped_keeper_is_killed_and_reaped() {
        let launcher = Cat::new();
        let keeper = Keeper::watch(&launcher, Path::new("/k")).expect("keeper");
        let pid = keeper.child.id();
        let proc = std::path::PathBuf::from(format!("/proc/{pid}"));
        assert!(proc.exists(), "the keeper is not running");
        drop(keeper);
        assert!(!proc.exists(), "the dropped keeper is still there");
    }
}
