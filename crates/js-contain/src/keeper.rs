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
//! The keeper process itself is the embedding binary's own mode, described by a
//! [`KeeperLauncher`]; this module starts it and owns it. The handle puts the keeper in a
//! process group of its own (a signal sent to the daemon's group does not reach it) and
//! waits for the process to say it runs as a keeper before it returns. The process must
//! ignore SIGHUP, SIGINT and SIGTERM, so a service manager that stops the daemon's unit
//! with SIGTERM does not stop it before it has read end-of-file. The daemon removes the
//! keeper (SIGKILL, then reaps it) once the lease is cleaned.

use std::io::{self, Read as _};
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::{Child, Command, Stdio};

use crate::oci::JobDrop;

/// How many times, a second apart, the keeper writes `cgroup.kill`.
pub const KILL_TRIES: u32 = 30;

/// What the embedding binary supplies: the keeper process and how it says it is ready.
pub trait KeeperLauncher {
    /// The command line of a keeper that, once the end of its stdin that the daemon holds
    /// is closed, writes `1` to the file `kill` up to `tries` times a second apart. It must
    /// ignore SIGHUP, SIGINT and SIGTERM and write [`KeeperLauncher::ready`] to its stdout
    /// once it does. The handle sets its stdin and stdout to pipes, its stderr to null
    /// and its process group; the launcher sets the rest.
    fn command(&self, kill: &Path, tries: u32) -> Command;

    /// What the keeper writes to its stdout when it runs as a keeper.
    fn ready(&self) -> &[u8];
}

/// One lease's keeper. Dropped, it is SIGKILLed and reaped without writing anything:
/// the pipe it reads closes only after it is dead.
#[derive(Debug)]
pub struct Keeper {
    child: Child,
}

impl Keeper {
    /// Starts the keeper of the cgroup whose `cgroup.kill` is `kill`, with `launcher`
    /// ([`KILL_TRIES`] tries), in a process group of its own, and returns once it has
    /// said it runs as a keeper.
    ///
    /// # Errors
    /// The keeper could not be started, or its program did not answer as a keeper (the
    /// process is killed and reaped).
    pub fn watch(launcher: &impl KeeperLauncher, kill: &Path) -> io::Result<Self> {
        Self::watch_as(launcher, kill, None)
    }

    /// [`Keeper::watch`] for a root daemon: with `job` (from
    /// [`Oci::job_drop`](crate::Oci::job_drop)) the keeper runs as the job user, dropped as
    /// every `crun` is (`setgroups([])`, the gid, the uid, from `/`, with an environment of
    /// `PATH` and `HOME=/` only). It then needs write access to the `kill` file, which
    /// this crate never gives it: a caller that starts a keeper this way hands over that one
    /// file (`cgroup.kill` of the lease's cgroup, by descriptor, as the agent's runtime hands
    /// over the files of [`JOB_OWNED_FILES`](crate::cgroup::JOB_OWNED_FILES), which leave it
    /// out on purpose). The agent's `CrunRuntime` starts no keeper: the agent writes the
    /// lease's `cgroup.kill` itself.
    ///
    /// # Errors
    /// As [`Keeper::watch`], and the drop failing in the child.
    pub fn watch_as(
        launcher: &impl KeeperLauncher,
        kill: &Path,
        job: Option<&JobDrop>,
    ) -> io::Result<Self> {
        let mut command = launcher.command(kill, KILL_TRIES);
        prepare(&mut command, job);
        let child = command.spawn().map_err(cannot_start)?;
        // Dropped on an error below, the keeper is killed and reaped.
        let mut this = Self { child };
        let ready = launcher.ready();
        let mut answer = vec![0; ready.len()];
        // Piped above, so there is a stdout to read.
        let stdout = this.child.stdout.as_mut().expect("stdout is piped");
        let read = stdout.read_exact(&mut answer);
        if read.is_err() || answer != ready {
            return Err(cannot_start(io::Error::other(
                "the keeper's program did not answer as a keeper",
            )));
        }
        Ok(this)
    }

    /// What a daemon's death does: closes the daemon's end of the keeper's stdin.
    /// The keeper stays this process's child, reaped on drop.
    #[cfg(test)]
    pub fn orphan(&mut self) {
        drop(self.child.stdin.take());
    }
}

/// Sets up the keeper's command line after the launcher has: the drop to the job user
/// (which clears the environment, so it comes before nothing that sets any), the pipes and
/// the process group.
fn prepare(command: &mut Command, job: Option<&JobDrop>) {
    job.into_iter().for_each(|job| job.apply(command));
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
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

    /// A launcher whose keeper is a stand-in program that holds its stdin pipe and ends
    /// when the pipe closes, as a keeper does. It records what it was asked.
    struct Stand {
        program: &'static str,
        args: &'static [&'static str],
        ready: &'static [u8],
        asked: RefCell<Vec<(std::path::PathBuf, u32)>>,
    }

    impl Stand {
        /// `cat`, which says nothing: ready is the empty answer.
        fn cat() -> Self {
            Self::of("cat", &[], b"")
        }

        fn of(program: &'static str, args: &'static [&'static str], ready: &'static [u8]) -> Self {
            Self {
                program,
                args,
                ready,
                asked: RefCell::default(),
            }
        }
    }

    impl KeeperLauncher for Stand {
        fn command(&self, kill: &Path, tries: u32) -> Command {
            self.asked.borrow_mut().push((kill.to_owned(), tries));
            let mut command = Command::new(self.program);
            command.args(self.args);
            command
        }

        fn ready(&self) -> &[u8] {
            self.ready
        }
    }

    /// Catches a keeper that is not handed the cgroup it is to kill or the number of
    /// tries, a handle that stops its process while the daemon still holds the pipe (the
    /// keeper must outlive the lease's running), and one that does not end it once the
    /// pipe closes (the stand-in ends at end-of-file, as the keeper does before it writes; the
    /// test waits for that end, so a keeper that went on would hang it).
    #[test]
    fn a_keeper_runs_while_held_and_ends_when_its_pipe_closes() {
        let launcher = Stand::cat();
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

    /// Catches a keeper started without the job-user drop, or with the launcher's
    /// environment kept: with a drop the command line has the drop's working directory and an
    /// environment of `PATH` and `HOME=/` alone (the launcher's `TOKEN` is gone), and without
    /// one the launcher's command is left as it made it.
    #[test]
    fn the_keeper_command_gets_the_job_drops_environment_only_when_there_is_a_drop() {
        let drop = JobDrop::new(Some((900, 901)), Some("/id/tools".as_ref()), 0).expect("drop");
        let made = || {
            let mut command = Command::new("true");
            command.env("TOKEN", "x");
            command
        };
        let mut dropped = made();
        prepare(&mut dropped, Some(&drop));
        let envs: Vec<_> = dropped.get_envs().collect();
        assert_eq!(
            envs,
            [
                (
                    std::ffi::OsStr::new("HOME"),
                    Some(std::ffi::OsStr::new("/"))
                ),
                (
                    std::ffi::OsStr::new("PATH"),
                    Some(std::ffi::OsStr::new("/id/tools"))
                ),
            ]
        );
        assert_eq!(dropped.get_current_dir(), Some(Path::new("/")));
        let mut plain = made();
        prepare(&mut plain, None);
        let envs: Vec<_> = plain.get_envs().collect();
        assert_eq!(
            envs,
            [(
                std::ffi::OsStr::new("TOKEN"),
                Some(std::ffi::OsStr::new("x"))
            )]
        );
        assert_eq!(plain.get_current_dir(), None);
    }

    /// Catches a drop that is built into the keeper's command line but never run in the
    /// child: the drop's `setgroups` needs privilege, so as an ordinary user the keeper fails
    /// to start with the kernel's refusal, and as root it starts (the same expression says
    /// which, from whether this process is root).
    #[test]
    fn a_keeper_started_with_the_drop_runs_it_in_the_child() {
        let is_root = rustix::process::getuid().is_root();
        let (uid, gid) = (
            rustix::process::getuid().as_raw(),
            rustix::process::getgid().as_raw(),
        );
        let drop = JobDrop::new(Some((uid, gid)), Some("/usr/bin:/bin".as_ref()), 0).expect("drop");
        let started = Keeper::watch_as(&Stand::cat(), Path::new("/k"), Some(&drop));
        assert_eq!(started.is_ok(), is_root, "{started:?}");
        let plain = Keeper::watch_as(&Stand::cat(), Path::new("/k"), None);
        assert!(plain.is_ok(), "without a drop nothing is refused");
    }

    /// Catches a keeper that cannot be started taken for one that runs, and an error
    /// that does not say what failed.
    #[test]
    fn a_keeper_that_cannot_start_is_an_error() {
        let launcher = Stand::of("/nonexistent-keeper/program", &[], b"");
        let error = Keeper::watch(&launcher, Path::new("/k")).expect_err("no program");
        assert_eq!(error.kind(), io::ErrorKind::NotFound, "{error}");
        assert!(
            error.to_string().starts_with("start the lease's keeper: "),
            "{error}"
        );
    }

    /// Catches the readiness wait missing from the handle: a program that does not say
    /// what the launcher says it says is no keeper (`printf` says `other`, not `ready`),
    /// the process is reaped, and the one that does say it is accepted.
    #[test]
    fn a_program_that_does_not_answer_as_a_keeper_is_an_error() {
        let wrong = Stand::of("printf", &["other\n"], b"ready\n");
        let error = Keeper::watch(&wrong, Path::new("/k")).expect_err("not a keeper");
        assert!(
            error.to_string().contains("did not answer as a keeper"),
            "{error}"
        );
        let silent = Stand::of("true", &[], b"ready\n");
        assert!(
            Keeper::watch(&silent, Path::new("/k")).is_err(),
            "no answer at all"
        );
        let right = Stand::of("printf", &["ready\n"], b"ready\n");
        Keeper::watch(&right, Path::new("/k")).expect("answers");
    }

    /// Catches a keeper left in the daemon's process group (a signal to the group would
    /// end it before the daemon's death does): its group is its own pid.
    #[test]
    fn the_keeper_is_in_a_process_group_of_its_own() {
        let launcher = Stand::cat();
        let keeper = Keeper::watch(&launcher, Path::new("/k")).expect("keeper");
        let pid = rustix::process::Pid::from_raw(i32::try_from(keeper.child.id()).expect("pid"))
            .expect("a pid");
        let group = rustix::process::getpgid(Some(pid)).expect("group");
        assert_eq!(group, pid);
        assert_ne!(group, rustix::process::getpgrp());
    }

    /// Catches a keeper whose drop (the daemon removing it once the lease is cleaned)
    /// leaves the process behind: dropped, it is killed and reaped before the drop
    /// returns, so its pid is gone.
    #[test]
    fn a_dropped_keeper_is_killed_and_reaped() {
        let launcher = Stand::cat();
        let keeper = Keeper::watch(&launcher, Path::new("/k")).expect("keeper");
        let pid = keeper.child.id();
        let proc = std::path::PathBuf::from(format!("/proc/{pid}"));
        assert!(proc.exists(), "the keeper is not running");
        drop(keeper);
        assert!(!proc.exists(), "the dropped keeper is still there");
    }
}
