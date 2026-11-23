//! A fake of the agent's side of the supervisor ([`Launcher`]): it starts the run's program
//! itself, as the test's own user, with the environment, working directory and standard
//! streams the runtime gave, in a process group of its own, and keeps what it was asked.
//! It drops to no account and applies no profile: what the runtime decides is in the request,
//! and the tests read it there.

use std::io;
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use js_agent::mac_supervisor::{
    Ended, ExitRecord, LaunchRequest, Launcher, Ready, Step, Supervised,
};

/// What one launch was asked: the supervisor's spec, as fields, and what it inherits.
#[derive(Debug, Clone)]
pub struct Launched {
    /// The account's uid and group.
    pub uid: u32,
    pub gid: u32,
    /// Whether the supervisor opens an audit session of its own.
    pub new_session: bool,
    /// The profile's text and parameters.
    pub profile: String,
    pub params: Vec<(String, String)>,
    /// The program and its arguments.
    pub program: String,
    pub args: Vec<String>,
    /// The limits.
    pub max_file_bytes: u64,
    pub max_processes: u32,
    /// The directory of the supervisor's records.
    pub record_dir: PathBuf,
    /// The environment, in order: the supervisor's whole environment.
    pub env: Vec<(String, String)>,
    /// The working directory.
    pub cwd: PathBuf,
}

impl Launched {
    /// The value of the variable `name`, the last one when it is given twice.
    pub fn var(&self, name: &str) -> Option<&str> {
        self.env
            .iter()
            .rev()
            .find(|(given, _)| given == name)
            .map(|(_, value)| value.as_str())
    }
}

/// How the next launches fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failing {
    /// They do not.
    No,
    /// The supervisor cannot be started.
    Launch,
    /// The supervisor starts and fails a setup step: no job starts.
    Setup,
    /// The job starts (as a real process); the first look at its end finds it exited with
    /// status 7, and every later look finds nothing: the supervisor's record is read once.
    ExitOnce,
    /// The job starts (as a real process); the first look at its end finds that a setup step
    /// failed, so the end is no job's exit.
    SetupAtExit,
}

/// What the test changes while the supervisors it started run.
#[derive(Debug, Default)]
struct Controls {
    /// How many times any supervisor was looked at for its end.
    looks: AtomicUsize,
    /// Whether a look at a supervisor's end fails.
    ask_fails: AtomicBool,
}

#[derive(Debug)]
struct State {
    launched: Vec<Launched>,
    failing: Failing,
    /// The pid the next jobs report as theirs, when it is not their real pid.
    job_pid: Option<i32>,
    /// The pid of each program started, which is also its process group's id.
    groups: Vec<i32>,
}

/// The fake launcher. Shared by the runtime and the test.
#[derive(Debug)]
pub struct FakeLauncher {
    state: Mutex<State>,
    controls: Arc<Controls>,
}

impl FakeLauncher {
    /// A launcher whose launches start their program.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                launched: Vec::new(),
                failing: Failing::No,
                job_pid: None,
                groups: Vec::new(),
            }),
            controls: Arc::new(Controls::default()),
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The next launches fail as `failing` says.
    pub fn fail(&self, failing: Failing) {
        self.state().failing = failing;
    }

    /// The next jobs report `pid` as their pid (and so their process group), in place of
    /// their real one.
    pub fn report_job_pid(&self, pid: i32) {
        self.state().job_pid = Some(pid);
    }

    /// Whether a look at a supervisor's end fails from now on.
    pub fn fail_looks(&self, fails: bool) {
        self.controls.ask_fails.store(fails, Ordering::SeqCst);
    }

    /// How many times a supervisor was looked at for its end, by a poll or a removal.
    pub fn looks(&self) -> usize {
        self.controls.looks.load(Ordering::SeqCst)
    }

    /// Every launch asked, in order.
    pub fn launched(&self) -> Vec<Launched> {
        self.state().launched.clone()
    }

    /// The process group of every program a launch started, in order: the job's pid.
    pub fn groups(&self) -> Vec<i32> {
        self.state().groups.clone()
    }

    /// The last launch asked.
    pub fn last(&self) -> Launched {
        self.launched()
            .pop()
            .expect("the runtime launched no supervisor")
    }
}

impl Launcher for FakeLauncher {
    fn launch(&self, request: LaunchRequest<'_>) -> io::Result<Box<dyn Supervised>> {
        let spec = request.spec;
        let launched = Launched {
            uid: spec.uid,
            gid: spec.gid,
            new_session: spec.new_session,
            profile: spec.profile,
            params: spec.params,
            program: spec.program,
            args: spec.args,
            max_file_bytes: spec.max_file_bytes,
            max_processes: spec.max_processes,
            record_dir: PathBuf::from(spec.record_dir),
            env: request.env.to_vec(),
            cwd: request.cwd.to_path_buf(),
        };
        let (failing, job_pid) = {
            let mut state = self.state();
            state.launched.push(launched.clone());
            (state.failing, state.job_pid)
        };
        let controls = self.controls.clone();
        match failing {
            Failing::Launch => Err(io::Error::other("the supervisor cannot be started")),
            Failing::Setup => Ok(Box::new(FakeSupervised {
                child: None,
                record_dir: launched.record_dir,
                controls,
                job_pid,
                mode: failing,
                gave: false,
            })),
            Failing::No | Failing::ExitOnce | Failing::SetupAtExit => {
                let [stdin, stdout, stderr] = request.stdio;
                let child = Command::new(&launched.program)
                    .args(&launched.args)
                    .env_clear()
                    .envs(launched.env.iter().cloned())
                    .current_dir(&launched.cwd)
                    .stdin(stdin)
                    .stdout(stdout)
                    .stderr(stderr)
                    .process_group(0)
                    .spawn()?;
                if let Ok(group) = i32::try_from(child.id()) {
                    self.state().groups.push(group);
                }
                Ok(Box::new(FakeSupervised {
                    child: Some(child),
                    record_dir: launched.record_dir,
                    controls,
                    job_pid,
                    mode: failing,
                    gave: false,
                }))
            }
        }
    }
}

/// A started fake supervisor: the job's process, or none when its setup failed.
struct FakeSupervised {
    child: Option<Child>,
    record_dir: PathBuf,
    controls: Arc<Controls>,
    /// The pid to report as the job's, in place of the child's.
    job_pid: Option<i32>,
    /// How the launch was told to fail.
    mode: Failing,
    /// Whether the one record of [`Failing::ExitOnce`] or [`Failing::SetupAtExit`] was given.
    gave: bool,
}

impl Supervised for FakeSupervised {
    fn pid(&self) -> u32 {
        self.child.as_ref().map_or(0, Child::id)
    }

    fn wait_ready(&mut self) -> Result<Ready, Ended> {
        match &self.child {
            None => Err(Ended::SetupFailed {
                step: Step::Profile,
                why: "the profile was refused".to_owned(),
            }),
            Some(child) => Ok(Ready {
                job_pid: self
                    .job_pid
                    .unwrap_or_else(|| i32::try_from(child.id()).unwrap_or(i32::MAX)),
                asid: None,
            }),
        }
    }

    fn try_exit(&mut self) -> io::Result<Option<ExitRecord>> {
        self.controls.looks.fetch_add(1, Ordering::SeqCst);
        if self.controls.ask_fails.load(Ordering::SeqCst) {
            return Err(io::Error::other("the supervisor cannot be asked"));
        }
        let first = !std::mem::replace(&mut self.gave, true);
        match (self.mode, first) {
            (Failing::ExitOnce, true) => {
                return Ok(Some(ExitRecord {
                    ended: Ended::Exited(7),
                    job_pid: None,
                    asid: None,
                }));
            }
            (Failing::ExitOnce, false) => return Ok(None),
            (Failing::SetupAtExit, _) => {
                return Ok(Some(ExitRecord {
                    ended: Ended::SetupFailed {
                        step: Step::Profile,
                        why: "the profile was refused".to_owned(),
                    },
                    job_pid: None,
                    asid: None,
                }));
            }
            _ => {}
        }
        let Some(child) = &mut self.child else {
            return Ok(None);
        };
        let job_pid = i32::try_from(child.id()).ok();
        Ok(child.try_wait()?.map(|status| ExitRecord {
            ended: match (status.code(), status.signal()) {
                (Some(code), _) => Ended::Exited(code),
                (None, Some(signal)) => Ended::Signalled(signal),
                (None, None) => Ended::Exited(255),
            },
            job_pid,
            asid: None,
        }))
    }

    fn record_dir(&self) -> &Path {
        &self.record_dir
    }
}
