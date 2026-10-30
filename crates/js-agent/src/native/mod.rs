//! The runtime of native jobs: a program run on a Mac as a user of its own, under a
//! Seatbelt profile, through the root helper `js-mac-session`.
//!
//! The helper owns everything that needs root. One run uses four of its verbs, each on a
//! connection of its own:
//!
//! 1. `user-create` makes the run's user, a fresh one named for the run's lease id
//!    (`<term>.<sequence>`: the term is drawn when the runtime is made and the sequence
//!    counts up, so a name is never used twice, which the helper's ledger also demands).
//!    The reply is the user's uid, taken from the helper's range.
//! 2. The agent, as root and before any process of that user exists, makes the run
//!    directory below the user's home (`exec/`, the working directory, and `tmp/`) and
//!    gives it to the user. It opens the job's two log files in the lifecycle's scratch
//!    directory, which only the agent can enter.
//! 3. `run` starts `sandbox-exec -D ... -p <profile> <program> <args>` as the user, with
//!    standard input from `/dev/null`, the log files as standard output and error, and
//!    `exec/` as the working directory. The profile is built by
//!    [`js_mac_pool::seatbelt`] from a policy that grants writes to the home and nothing
//!    else, and the helper builds the process's environment around the one the agent
//!    passes. The connection stays open until the process exits: a thread of the runtime
//!    waits on it, and [`Runtime::poll`] reads what it found, so it never blocks.
//! 4. `kill-uid` ends every process of the user, not only the one started (a job may
//!    detach children), and `user-delete` removes the user and sweeps its home.
//!
//! What this runtime does not do yet: stage inputs or collect outputs (a start that names
//! either is refused `native-unsupported`), bound the size of the log files, adopt a run
//! after the agent restarts (the helper's boot reset, asked for when the runtime is made,
//! ends the runs a previous agent left when the helper has just started), or grant the
//! user's `/var/folders` entry (toolchains that need it get their temporary files from
//! `TMPDIR`, which is inside the home).
//!
//! The runtime builds on every platform so that its rules are tested on Linux; only a
//! macOS agent makes one.

mod logs;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt::Display;
use std::fs::{self, File};
use std::io;
use std::os::fd::AsFd as _;
use std::os::unix::fs::{PermissionsExt as _, chown};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::thread;
use std::time::Duration;

use js_agent_wire::{LogTail, NativeSpec, NetworkLevel, refusal};
use js_mac_pool::seatbelt::{
    AbsPath, Caps, Narrowing, Net, Policy, Profile, ReadModel, SlotLayout,
};
use js_mac_session::client::{Client, ClientError, Exit as HelperExit};
use js_mac_session::helper::RESET_ALREADY;
use js_mac_session::lease::{Dotted, user_name};
use js_types::LeaseId;

use crate::lifecycle::{
    Body, Exit, Found, NativeRefusal, RunHandle, RunSpec, Runtime, RuntimeError, Status,
};

/// What a native runtime is told, from the agent's flags.
#[derive(Debug, Clone)]
pub struct NativeConfig {
    /// Where the helper makes the home of a run's user (its `--homes`), canonical: a
    /// profile compares the path the kernel resolves, and `/var` is `/private/var`.
    pub homes: PathBuf,
    /// The most network any run gets; a start asks for less or the same.
    pub net: Net,
    /// The directories a start's program must lie under.
    pub tool_roots: Vec<PathBuf>,
    /// Paths no run may read, such as the agent's state and scratch roots.
    pub read_denies: Vec<AbsPath>,
}

/// A run that started: its scratch directory (where its log files are) and what the thread
/// waiting on the helper found.
struct Run {
    scratch: PathBuf,
    /// Set once the run has ended, so it is answered the same way again.
    ended: Option<Status>,
    /// The exit, or `None` when the helper's connection ended without one.
    waiter: Receiver<Option<Exit>>,
}

/// Runs native jobs as users of the root helper.
pub struct NativeRuntime {
    helper: Client,
    config: NativeConfig,
    term: u64,
    next: u64,
    runs: BTreeMap<String, Run>,
}

impl NativeRuntime {
    /// A runtime over the helper `helper`. Its run ids are `term.0`, `term.1` and so on, so
    /// a `term` that another runtime of the same helper has used would be refused by the
    /// helper's ledger: draw it at random.
    ///
    /// The helper refuses every verb until its boot reset has run since it started, so the
    /// reset is asked for here, naming no lease held: it ends the runs of an earlier agent.
    /// A helper that has reset already answers so, and that is fine.
    ///
    /// # Errors
    ///
    /// The helper cannot be reached or its boot reset failed.
    pub fn open(helper: Client, config: NativeConfig, term: u64) -> Result<Self, RuntimeError> {
        match helper.boot_reset(&[]) {
            Ok(_) => {}
            Err(ClientError::Refused(why)) if why == RESET_ALREADY => {}
            Err(why) => return Err(fail(&"the boot reset", &why)),
        }
        Ok(Self {
            helper,
            config,
            term,
            next: 0,
            runs: BTreeMap::new(),
        })
    }

    /// The `sandbox-exec` command line of `native`, run as a user whose home is `home`.
    fn argv(&self, native: &NativeSpec, home: &Path) -> Result<Vec<String>, RuntimeError> {
        let run = AbsPath::new(home).map_err(|why| fail(&home.display(), &why))?;
        let layout = SlotLayout {
            run,
            uid_entry: None,
            inputs: Vec::new(),
            read_denies: self.config.read_denies.clone(),
        };
        let caps = Caps {
            net: self.config.net.clone(),
            read_model: ReadModel::Open,
        };
        let narrowing = Narrowing {
            net: Some(net_of(native.network)),
            ..Narrowing::default()
        };
        let policy = Policy::base(layout, caps)
            .and_then(|policy| policy.narrowed(narrowing))
            .map_err(|why| fail(&"the policy", &why))?;
        let profile = Profile::for_policy(&policy).map_err(|why| fail(&"the profile", &why))?;
        let program =
            AbsPath::new(Path::new(&native.program)).map_err(|why| fail(&native.program, &why))?;
        let args: Vec<OsString> = native.args.iter().map(OsString::from).collect();
        let argv = profile.argv(&program, &args);
        Ok(argv
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect())
    }

    /// Everything after the user exists: the run directory, the log files, the profile and
    /// the `run` verb.
    fn launch(
        &self,
        spec: &RunSpec,
        native: &NativeSpec,
        (id, lease, uid): (LeaseId, &str, u32),
    ) -> Result<Run, RuntimeError> {
        let home = self.config.homes.join(user_name(id));
        let (exec, tmp) = (home.join("exec"), home.join("tmp"));
        for dir in [&exec, &tmp] {
            make_dir(dir, uid).map_err(|why| fail(&dir.display(), &why))?;
        }
        let argv = self.argv(native, &home)?;
        let mut env: Vec<(String, String)> = native
            .env
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
        env.push(("TMPDIR".to_owned(), tmp.display().to_string()));
        let stdin = File::open("/dev/null").map_err(|why| fail(&"/dev/null", &why))?;
        let out = spec.scratch.join(logs::STDOUT);
        let stdout = File::create(&out).map_err(|why| fail(&out.display(), &why))?;
        let err = spec.scratch.join(logs::STDERR);
        let stderr = File::create(&err).map_err(|why| fail(&err.display(), &why))?;
        let cwd = File::open(&exec).map_err(|why| fail(&exec.display(), &why))?;
        let fds = [stdin.as_fd(), stdout.as_fd(), stderr.as_fd(), cwd.as_fd()];
        let running = self
            .helper
            .run(lease, &argv, &env, fds)
            .map_err(|why| fail(&"run", &why))?;
        let (sender, waiter) = channel();
        thread::spawn(move || {
            let _ = sender.send(running.wait().ok().map(exit_of));
        });
        Ok(Run {
            scratch: spec.scratch.clone(),
            ended: None,
            waiter,
        })
    }

    /// Ends the user of `lease` after a start that failed, so none is left behind. A
    /// failure to do so is not reported: the start's own error is the one the operator
    /// needs, and the helper's boot reset ends what is left.
    fn discard(&self, lease: &str) {
        let _ = self.helper.kill_uid(lease);
        let _ = self.helper.user_delete(lease);
    }
}

impl Runtime for NativeRuntime {
    fn create_and_start(&mut self, spec: &RunSpec) -> Result<RunHandle, RuntimeError> {
        let Body::Native(native) = &spec.body else {
            return Err(RuntimeError::new("this runtime runs native bodies only"));
        };
        let id = LeaseId::new(self.term, self.next);
        self.next += 1;
        let lease = Dotted(id).to_string();
        let uid = self
            .helper
            .user_create(&lease, None)
            .map_err(|why| fail(&"user-create", &why))?;
        match self.launch(spec, native, (id, &lease, uid)) {
            Ok(run) => {
                self.runs.insert(lease.clone(), run);
                Ok(RunHandle(lease))
            }
            Err(why) => {
                self.discard(&lease);
                Err(why)
            }
        }
    }

    fn poll(&mut self, id: &RunHandle) -> Result<Status, RuntimeError> {
        let Some(run) = self.runs.get_mut(&id.0) else {
            return Ok(Status::Gone);
        };
        if run.ended.is_none() {
            run.ended = match run.waiter.try_recv() {
                Ok(Some(exit)) => Some(Status::Exited(exit)),
                // The helper's connection ended without an exit, or its waiter is gone:
                // nothing says what became of the process, and removing the run ends it.
                Ok(None) | Err(TryRecvError::Disconnected) => Some(Status::Gone),
                Err(TryRecvError::Empty) => None,
            };
        }
        Ok(run.ended.unwrap_or(Status::Running))
    }

    /// Kills at once: the helper's only way to end a user's processes is `SIGKILL`, so
    /// `grace` is not used.
    fn stop(&mut self, id: &RunHandle, _grace: Duration) -> Result<(), RuntimeError> {
        self.helper
            .kill_uid(&id.0)
            .map_err(|why| fail(&"kill-uid", &why))
    }

    fn remove(&mut self, id: &RunHandle) -> Result<(), RuntimeError> {
        self.helper
            .kill_uid(&id.0)
            .map_err(|why| fail(&"kill-uid", &why))?;
        self.helper
            .user_delete(&id.0)
            .map_err(|why| fail(&"user-delete", &why))?;
        self.runs.remove(&id.0);
        Ok(())
    }

    /// Lists nothing: a run is not adopted by a later process of the agent.
    fn list_labelled(&mut self) -> Result<Vec<Found>, RuntimeError> {
        Ok(Vec::new())
    }

    fn log_tail(&mut self, id: &RunHandle) -> Result<Option<LogTail>, RuntimeError> {
        match self.runs.get(&id.0) {
            None => Ok(None),
            Some(run) => logs::tail(&run.scratch)
                .map(Some)
                .map_err(|why| fail(&"the log", &why)),
        }
    }

    fn supports_gpus(&self) -> bool {
        false
    }

    fn check_native(&self, native: &NativeSpec) -> Result<(), NativeRefusal> {
        let staged = [
            !native.inputs.is_empty(),
            !native.outputs.is_empty(),
            !native.cwd.is_empty(),
        ];
        if staged.into_iter().any(|named| named) {
            return Err(NativeRefusal {
                code: refusal::NATIVE_UNSUPPORTED,
                why: "this agent stages no inputs, collects no outputs and runs in no \
                      subdirectory yet"
                    .to_owned(),
            });
        }
        let program = Path::new(&native.program);
        if !self
            .config
            .tool_roots
            .iter()
            .any(|root| program.starts_with(root))
        {
            return Err(NativeRefusal {
                code: refusal::INVALID_PROGRAM,
                why: format!("{} lies under no tool root", native.program),
            });
        }
        Ok(())
    }
}

/// An error of `what`, for the operator's log.
fn fail(what: &dyn Display, why: &dyn Display) -> RuntimeError {
    RuntimeError::new(format!("{what}: {why}"))
}

/// The network level of a start as the profile's.
fn net_of(level: NetworkLevel) -> Net {
    match level {
        NetworkLevel::Off => Net::Off,
        NetworkLevel::Loopback => Net::Loopback,
        NetworkLevel::On => Net::On,
    }
}

/// How a process ended, as a container's exit is told: a process that a signal ended has
/// the status a shell would give it (128 and the signal).
fn exit_of(ended: HelperExit) -> Exit {
    let signalled = ended.signal.map(|signal| 128 + signal);
    Exit {
        code: ended.code.or(signalled).unwrap_or(255),
        oom_killed: false,
    }
}

/// Makes the directory `path`, private to `uid`. No process of that user exists yet, so
/// nothing can race the steps.
fn make_dir(path: &Path, uid: u32) -> io::Result<()> {
    fs::create_dir(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    chown(path, Some(uid), None)
}
