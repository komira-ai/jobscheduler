//! The command of `lease run`: a child in a process group of its own, which `js` signals
//! as a whole and leaves no member of running.

#[cfg(target_os = "linux")]
use std::io;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, ExitStatus};
use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;

use rustix::io::Errno;
#[cfg(target_os = "linux")]
use rustix::process::set_parent_process_death_signal;
use rustix::process::{
    Pid, Signal, WaitId, WaitIdOptions, kill_process_group, test_kill_process_group, waitid,
};

use crate::api::Lease;
use crate::cli::RunArgs;
use crate::events::{Event, Events, Waiter};
use crate::failure::{Failure, exit};

/// How often the group is looked at while what the command left behind ends after
/// SIGTERM. The grace period is spent in steps of this, each asked of the waiter.
pub(crate) const POLL: Duration = Duration::from_millis(100);

/// The command, running in a process group of its own so that a signal reaches what it
/// started as well. Dropping it before [`Running::sweep`] has settled the group (an
/// unwind) kills the group and reaps the command.
pub(crate) struct Running {
    child: Child,
    group: Pid,
    settled: bool,
}

impl Running {
    /// Starts `args.command` with the lease in its environment. On Linux the command is
    /// killed (SIGKILL) when the thread that started it ends, `js` being killed
    /// included; what the command started itself is not.
    pub(crate) fn start(args: &RunArgs, lease: &Lease) -> Result<Self, Failure> {
        let program = args.command.first().cloned().unwrap_or_default();
        let mut command = Command::new(&program);
        command
            .args(args.command.iter().skip(1))
            .env("JS_LEASE_ID", &lease.id)
            .env("JS_LEASE_HOST", &lease.host)
            .process_group(0);
        end_with_parent(&mut command);
        let child = command.spawn().map_err(|why| Failure::NotStarted {
            program: program.to_string_lossy().into_owned(),
            why: why.to_string(),
        })?;
        let group = Pid::from_child(&child);
        Ok(Self {
            child,
            group,
            settled: false,
        })
    }

    /// The process group the command leads.
    pub(crate) fn group(&self) -> Pid {
        self.group
    }

    /// Sends `signal` to the command's whole process group.
    pub(crate) fn signal(&self, signal: Signal) {
        let _ = kill_process_group(self.group, signal);
    }

    /// Reaps the command and turns how it ended into the status `js` exits with: its
    /// own, or 128 plus the signal that ended it.
    pub(crate) fn reap(&mut self) -> u8 {
        self.child
            .wait()
            .map_or(exit::SERVER, |status| status_code(&status))
    }

    /// Ends what is left of the group once the command has been reaped: nothing when no
    /// process is left, else SIGTERM and then, when the group still has a member after
    /// `grace` (counted in steps of [`POLL`], each asked of `waiter`) or when `js` is
    /// sent a signal meanwhile, SIGKILL. The process group id is free for reuse only
    /// once its last member is gone, so it is signalled only while one is left.
    pub(crate) fn sweep(&mut self, grace: Duration, waiter: &dyn Waiter, events: &Events) {
        if self.group_alive() {
            self.signal(Signal::TERM);
        }
        let mut waited = Duration::ZERO;
        while self.waiting(waited, grace, events) {
            waiter.wait(POLL);
            waited += POLL;
        }
        if self.group_alive() {
            self.signal(Signal::KILL);
        }
        self.settled = true;
    }

    /// Whether the sweep goes on waiting for the group to end on its own: it has a member,
    /// the grace period is not spent and no signal has come.
    fn waiting(&self, waited: Duration, grace: Duration, events: &Events) -> bool {
        let checks = [
            self.group_alive(),
            waited < grace,
            events.interrupted().is_none(),
        ];
        checks.into_iter().all(|holds| holds)
    }

    /// Whether any process is left in the group.
    fn group_alive(&self) -> bool {
        !matches!(test_kill_process_group(self.group), Err(Errno::SRCH))
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if !self.settled {
            self.signal(Signal::KILL);
            let _ = self.child.wait();
        }
    }
}

/// Asks the system to kill the command when the thread that starts it ends: a `js` that
/// is killed leaves no command running. Linux only. A `js` that ends between the fork and
/// the exec leaves the command running: the lease expires with its ttl.
#[cfg(target_os = "linux")]
fn end_with_parent(command: &mut Command) {
    // SAFETY: `arm_death_signal` runs in the forked child before the exec, where only
    // async-signal-safe calls are allowed: it makes one system call (`prctl`) and
    // allocates nothing, an error being an OS error code.
    unsafe {
        command.pre_exec(arm_death_signal);
    }
}

/// What the forked child does before the exec: asks to be sent SIGKILL when its parent
/// ends.
#[cfg(target_os = "linux")]
fn arm_death_signal() -> io::Result<()> {
    set_parent_process_death_signal(Some(Signal::KILL)).map_err(Into::into)
}

/// Other systems have no such signal; a `js` that is killed leaves the command running.
#[cfg(not(target_os = "linux"))]
fn end_with_parent(_command: &mut Command) {}

fn status_code(status: &ExitStatus) -> u8 {
    let by_signal = status.signal().map(|signal| 128 + signal);
    u8::try_from(status.code().or(by_signal).unwrap_or(1)).unwrap_or(1)
}

/// Reports `Exited` once the command has exited, without reaping it: the status stays
/// for `reap`, and the process id cannot be reused before it is read.
pub(crate) fn watch_exit(pid: Pid, events: Sender<Event>) {
    thread::spawn(move || {
        let options = WaitIdOptions::EXITED | WaitIdOptions::NOWAIT;
        let _ = retry_interrupted(|| waitid(WaitId::Pid(pid), options));
        let _ = events.send(Event::Exited);
    });
}

/// Calls `call` until it is not cut short by a signal handler (`EINTR`).
fn retry_interrupted<T>(mut call: impl FnMut() -> Result<T, Errno>) -> Result<T, Errno> {
    loop {
        match call() {
            Err(Errno::INTR) => {}
            done => return done,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts that a call cut short by a signal is made again until it gives another
    /// answer, and that any other error is the answer.
    #[test]
    fn an_interrupted_call_is_made_again() {
        let mut answers = vec![Err(Errno::INTR), Err(Errno::INTR), Ok(7)];
        let got = retry_interrupted(|| answers.remove(0));
        assert_eq!(got, Ok(7));
        assert!(answers.is_empty());
        let mut failing = vec![Err(Errno::INTR), Err(Errno::CHILD)];
        let got: Result<u8, Errno> = retry_interrupted(|| failing.remove(0));
        assert_eq!(got, Err(Errno::CHILD));
    }
}

#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    use super::*;

    /// Asserts that what the forked child does before the exec succeeds. It is run here
    /// in the test process, which is then asked to be killed when its parent ends (the
    /// test runner outlives it), and the request is taken back at once.
    #[test]
    fn the_death_signal_can_be_armed() {
        assert!(arm_death_signal().is_ok());
        assert!(set_parent_process_death_signal(None).is_ok());
    }
}
