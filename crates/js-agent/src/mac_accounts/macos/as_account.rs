//! Running a program as an account, for `js-agent __account-cycle`.

use std::io;
use std::os::unix::process::ExitStatusExt as _;
use std::path::Path;
use std::process::Stdio;

use js_mac_session::disclaim::Program;

use super::super::Account;
use super::super::cycle::{Ran, Runner};

/// Runs programs as an account, started through the spawner every program of the agent goes
/// through ([`Program`]), so that the program answers for itself to the privacy controls and
/// holds none of the grant the agent holds.
pub(in super::super) struct AsAccount;

impl Runner for AsAccount {
    fn run(&self, account: &Account, home: &Path, command: &[String]) -> io::Result<Ran> {
        let (program, args) = command
            .split_first()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no program to run"))?;
        let (uid, gid) = (account.uid, account.gid);
        let steps = move || {
            let groups = [gid];
            // SAFETY: `groups` is ours and holds the 1 entry given; the other calls take plain
            // ids. Nothing here allocates: the child is a fork of a process that may have other
            // threads. The group list is set first (it needs root), then the ids; a process
            // that can still become root afterwards is refused.
            unsafe {
                if libc::setgroups(1, groups.as_ptr()) != 0
                    || libc::setgid(gid) != 0
                    || libc::setuid(uid) != 0
                {
                    return Err(io::Error::last_os_error());
                }
                if libc::setuid(0) == 0 {
                    return Err(io::Error::from_raw_os_error(libc::EPERM));
                }
            }
            Ok(())
        };
        // SAFETY: the steps call only async-signal-safe functions and allocate nothing, as
        // `with_steps` requires.
        let mut started = unsafe { Program::with_steps(program, args, steps) };
        started
            .env_clear()
            .env("HOME", home)
            .current_dir(home)
            .stdin(Stdio::null());
        let output = started.output()?;
        Ok(Ran {
            code: output.status.code(),
            signal: output.status.signal(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}
