//! What the tests of `lease run` use to know what its commands are doing without
//! sleeping or measuring time: named pipes that a command and the test meet at, and a
//! process id the test is told about the moment the process ends.

use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use rustix::event::{PollFd, PollFlags, poll};
use rustix::io::Errno;
use rustix::process::{Pid, PidfdFlags, pidfd_open, test_kill_process};

use crate::support::Scratch;

pub fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// Makes a named pipe in `scratch`: a command that writes to it tells the test it is
/// ready, and the test blocks on reading it, so nothing polls or sleeps.
pub fn fifo(scratch: &Scratch, name: &str) -> PathBuf {
    let path = scratch.path(name);
    let made = Command::new("mkfifo").arg(&path).status().unwrap();
    assert!(made.success());
    path
}

/// Asserts that no process has this id any more: it ended and its parent reaped it.
pub fn assert_reaped(pid: &str) {
    let pid = Pid::from_raw(pid.trim().parse().unwrap()).unwrap();
    assert_eq!(test_kill_process(pid), Err(Errno::SRCH));
}

pub fn path_str(path: &Path) -> &str {
    path.to_str().unwrap()
}

/// A handle on the process `pid` that says when it has ended, which is whether or not
/// anything has reaped it. Taken while the process still runs.
pub fn watch_pid(pid: &str) -> OwnedFd {
    let pid = Pid::from_raw(pid.trim().parse().unwrap()).unwrap();
    pidfd_open(pid, PidfdFlags::empty()).unwrap()
}

/// Returns when the process `watch_pid` was given has ended. A process nothing ends
/// keeps this waiting: the commands of the tests that use it end on their own after a
/// while and leave a mark that the test checks for, so a missing kill fails the test.
pub fn wait_gone(watch: &OwnedFd) {
    let mut fds = [PollFd::new(watch, PollFlags::IN)];
    poll(&mut fds, None).unwrap();
}
