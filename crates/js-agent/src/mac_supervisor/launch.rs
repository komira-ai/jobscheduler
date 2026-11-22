//! The agent's side of a supervised start: [`Launcher`] starts the supervisor for a
//! [`LaunchRequest`], and the [`Supervised`] it returns reads the supervisor's records. The real
//! launcher ([`SystemLauncher`]) makes one of the two starts that keep the responsibility
//! (`crates/xtask/src/spawn_sites.rs`, `KEEPERS`).

use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus, Stdio};
use std::time::Duration;

use js_mac_session::disclaim::{Kept, Program};

use super::record::{EXIT, Ended, ExitRecord, READY, Ready, SPEC, Step, from_status};
use super::record::{parse_exit, parse_ready};
use super::{SuperviseSpec, VERB};

/// The pause between two looks at the records while [`Supervised::wait_ready`] waits.
pub const PAUSE: Duration = Duration::from_millis(20);

/// One start: the spec, and what the supervisor inherits (it passes all of it to the job,
/// unchanged).
#[derive(Debug)]
pub struct LaunchRequest<'a> {
    /// The spec, written to `<record_dir>/spec.json`.
    pub spec: SuperviseSpec,
    /// The job's whole environment: the supervisor gets exactly this, and adds nothing.
    pub env: &'a [(String, String)],
    /// The job's working directory.
    pub cwd: &'a Path,
    /// The job's standard input, output and error.
    pub stdio: [Stdio; 3],
}

/// Starts supervisors.
pub trait Launcher: Send + Sync {
    /// Starts the supervisor of `request`. It has started, not yet started the job: wait for
    /// that with [`Supervised::wait_ready`].
    ///
    /// # Errors
    /// The spec could not be written, or the supervisor could not be started.
    fn launch(&self, request: LaunchRequest<'_>) -> io::Result<Box<dyn Supervised>>;
}

/// A started supervisor, read through its records.
pub trait Supervised: Send {
    /// The supervisor's pid (the job's is in its ready record).
    fn pid(&self) -> u32;

    /// Waits until the job has started (the ready record), or the supervisor has ended: then
    /// how the start ended (the exit record's end, or, with none, what the supervisor's own
    /// status says, [`from_status`]).
    ///
    /// # Errors
    /// The start ended without the job starting, as above.
    fn wait_ready(&mut self) -> Result<Ready, Ended>;

    /// The exit record, without waiting: `None` while the supervisor runs and has written
    /// none; once it has ended without one, a record made from its own status.
    ///
    /// # Errors
    /// The supervisor's status could not be read.
    fn try_exit(&mut self) -> io::Result<Option<ExitRecord>>;

    /// The record directory.
    fn record_dir(&self) -> &Path;
}

/// The supervisor's process, as [`Supervision`] looks at it.
pub trait SupervisorChild: Send {
    /// Its pid.
    fn pid(&self) -> u32;

    /// How it ended, or `None` while it runs; does not wait.
    ///
    /// # Errors
    /// It cannot be looked at.
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>>;
}

impl SupervisorChild for Child {
    fn pid(&self) -> u32 {
        self.id()
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        Child::try_wait(self)
    }
}

/// A started supervisor and its record directory: the [`Supervised`] of [`SystemLauncher`].
pub struct Supervision {
    /// The supervisor.
    child: Box<dyn SupervisorChild>,
    /// Its record directory.
    dir: PathBuf,
    /// What [`Supervised::wait_ready`] does between two looks.
    pause: Box<dyn Fn() + Send>,
    /// The ready record, once read.
    ready: Option<Ready>,
}

impl Supervision {
    /// The supervisor `child`, whose records are in `dir`; `pause` is what
    /// [`Supervised::wait_ready`] does between two looks ([`pause`] for the real one).
    #[must_use]
    pub fn new(child: Box<dyn SupervisorChild>, dir: PathBuf, pause: Box<dyn Fn() + Send>) -> Self {
        Self {
            child,
            dir,
            pause,
            ready: None,
        }
    }

    /// The ready record, when it has been written whole; kept once read.
    fn read_ready(&mut self) -> Option<Ready> {
        let ready = parse_ready(&text(&self.dir.join(READY)))?;
        self.ready = Some(ready);
        Some(ready)
    }

    /// The exit record, when it has been written whole.
    fn read_exit(&self) -> Option<ExitRecord> {
        parse_exit(&text(&self.dir.join(EXIT)))
    }

    /// The exit record of a supervisor that ended with `status`: its own, or one made from the
    /// status when it wrote none.
    fn ended(&self, status: ExitStatus) -> ExitRecord {
        self.read_exit().unwrap_or_else(|| ExitRecord {
            ended: from_status(status),
            job_pid: self.ready.map(|ready| ready.job_pid),
            asid: self.ready.and_then(|ready| ready.asid),
        })
    }
}

impl Supervised for Supervision {
    fn pid(&self) -> u32 {
        self.child.pid()
    }

    fn wait_ready(&mut self) -> Result<Ready, Ended> {
        loop {
            if let Some(ready) = self.read_ready() {
                return Ok(ready);
            }
            match self.child.try_wait() {
                Ok(None) => (self.pause)(),
                // The record may have been written between the look above and the end.
                Ok(Some(status)) => {
                    return self.read_ready().ok_or_else(|| self.ended(status).ended);
                }
                Err(why) => {
                    return Err(Ended::SetupFailed {
                        step: Step::Records,
                        why: format!("looking at the supervisor: {why}"),
                    });
                }
            }
        }
    }

    fn try_exit(&mut self) -> io::Result<Option<ExitRecord>> {
        if let Some(record) = self.read_exit() {
            return Ok(Some(record));
        }
        Ok(self.child.try_wait()?.map(|status| self.ended(status)))
    }

    fn record_dir(&self) -> &Path {
        &self.dir
    }
}

/// What a record file holds; empty when it cannot be read (not made yet).
fn text(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

/// The real pause of [`Supervised::wait_ready`]: [`PAUSE`].
pub fn pause() {
    std::thread::sleep(PAUSE);
}

/// Starts the supervisor as `<this program> __supervise-native --spec <record_dir>/spec.json`,
/// keeping the responsibility ([`Kept::Supervisor`]), so the supervisor, and the job it
/// starts, answer to the privacy controls for the agent. It writes the spec there first, a new
/// file of mode 0600; the record directory is the caller's to make, root's and mode 0700.
#[derive(Debug, Clone)]
pub struct SystemLauncher {
    /// The program started: this one.
    exe: PathBuf,
}

impl SystemLauncher {
    /// The launcher of this program ([`std::env::current_exe`]), as the agent uses it.
    ///
    /// # Errors
    /// The running program's path cannot be read.
    pub fn current() -> io::Result<Self> {
        Ok(Self::at(std::env::current_exe()?))
    }

    /// The launcher of the program `exe`, which must answer `__supervise-native` as this one
    /// does: a test starts the `js-agent` binary cargo built.
    #[must_use]
    pub fn at(exe: impl Into<PathBuf>) -> Self {
        Self { exe: exe.into() }
    }
}

impl Launcher for SystemLauncher {
    fn launch(&self, request: LaunchRequest<'_>) -> io::Result<Box<dyn Supervised>> {
        let dir = PathBuf::from(&request.spec.record_dir);
        let spec = dir.join(SPEC);
        write_spec(&spec, &request.spec)?;
        let [stdin, stdout, stderr] = request.stdio;
        let mut program = Program::new(
            &self.exe,
            [OsStr::new(VERB), OsStr::new("--spec"), spec.as_os_str()],
        );
        program
            .keep_responsibility(Kept::Supervisor)
            .env_clear()
            .envs(request.env.iter().cloned())
            .current_dir(request.cwd)
            .stdin(stdin)
            .stdout(stdout)
            .stderr(stderr)
            .process_group(0);
        let child = program.spawn().inspect_err(|_| {
            // Not started: nothing will read the spec, so it does not stay.
            let _ = fs::remove_file(&spec);
        })?;
        Ok(Box::new(Supervision::new(
            Box::new(child),
            dir,
            Box::new(pause),
        )))
    }
}

/// Writes `spec` to the new file `path`, mode 0600.
fn write_spec(path: &Path, spec: &SuperviseSpec) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(&serde_json::to_vec(spec)?)
}

#[cfg(test)]
#[path = "launch_tests.rs"]
mod tests;
