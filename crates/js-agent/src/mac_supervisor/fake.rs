//! [`FakeLauncher`]: a [`Launcher`] with no supervisor, for tests of the code that starts native
//! jobs. It is not behind `cfg(test)` because the agent's integration tests use it, and they
//! cannot see what only the library's own tests compile; nothing in the agent starts it.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Child;

use js_mac_session::disclaim::Program;
use rustix::process::Pid;

use super::launch::{LaunchRequest, Launcher, Supervised};
use super::record::{Ended, ExitRecord, Ready, Records, ended_of};

/// Runs the spec's `program` as shell text, `/bin/sh -c <program> sh <args...>`, as the
/// current user, with the request's environment, working directory and streams, in a process
/// group of its own, and writes the same records as the supervisor to the record directory:
/// the ready record as soon as the shell has started (no audit session, so its `asid` is
/// `None`), the exit record when [`Supervised::try_exit`] first sees the shell's end. It opens
/// no session, drops to no user, sets no limit and applies no profile, and the shell is
/// started disclaimed, as every program a test starts.
#[derive(Debug, Clone, Copy, Default)]
pub struct FakeLauncher;

impl Launcher for FakeLauncher {
    fn launch(&self, request: LaunchRequest<'_>) -> io::Result<Box<dyn Supervised>> {
        let dir = PathBuf::from(&request.spec.record_dir);
        let mut records = Records::open(&dir)?;
        let mut argv = vec!["-c", request.spec.program.as_str(), "sh"];
        argv.extend(request.spec.args.iter().map(String::as_str));
        let [stdin, stdout, stderr] = request.stdio;
        let mut shell = Program::new("/bin/sh", argv);
        shell
            .env_clear()
            .envs(request.env.iter().cloned())
            .current_dir(request.cwd)
            .stdin(stdin)
            .stdout(stdout)
            .stderr(stderr)
            .process_group(0);
        let child = shell.spawn()?;
        let ready = Ready {
            job_pid: Pid::from_child(&child).as_raw_nonzero().get(),
            asid: None,
        };
        records.write_ready(&ready)?;
        Ok(Box::new(FakeRun {
            child,
            records,
            dir,
            ready,
            exit: None,
        }))
    }
}

/// A shell [`FakeLauncher`] started.
struct FakeRun {
    child: Child,
    records: Records,
    dir: PathBuf,
    ready: Ready,
    /// The exit record, once written.
    exit: Option<ExitRecord>,
}

impl Supervised for FakeRun {
    /// The shell's pid: there is no supervisor.
    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn wait_ready(&mut self) -> Result<Ready, Ended> {
        Ok(self.ready)
    }

    fn try_exit(&mut self) -> io::Result<Option<ExitRecord>> {
        if self.exit.is_none() {
            let Some(status) = self.child.try_wait()? else {
                return Ok(None);
            };
            let record = ExitRecord {
                ended: ended_of(status),
                job_pid: Some(self.ready.job_pid),
                asid: None,
            };
            self.records.write_exit(&record)?;
            self.exit = Some(record);
        }
        Ok(self.exit.clone())
    }

    fn record_dir(&self) -> &Path {
        &self.dir
    }
}

#[cfg(test)]
#[path = "fake_tests.rs"]
mod tests;
