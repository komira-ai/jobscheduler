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
//!    gives it to the user. It makes the job's three standard streams as files in the
//!    lifecycle's scratch directory, which only the agent can enter: an empty one for
//!    standard input and one each for standard output and error.
//! 3. `run` starts `sandbox-exec -D ... -p <profile> <program> <args>` as the user, with
//!    the run's file-size and process limits as fields of the request: the helper sets
//!    them with `setrlimit` in the child, before it executes `sandbox-exec`, so no shell
//!    stands between the profile and the program. The helper starts it with
//!    the empty file as standard input, the log files as standard output and error, and
//!    `exec/` as the working directory. The profile is built by
//!    [`js_mac_pool::seatbelt`] from a policy that grants writes to the home and nothing
//!    else, and the helper builds the process's environment around the one the agent
//!    passes. The connection stays open until the process exits: a thread of the runtime
//!    waits on it, and [`Runtime::poll`] reads what it found, so it never blocks.
//!    A start with run files (`RunSpec::files`, written by the lifecycle into the agent's
//!    `--run-files-root`) has their directory given to the user before this, `0500` with
//!    its files `0400`; the profile denies writes there and `JS_FILES_DIR` names it
//!    (`native/files.rs`).
//! 4. `kill-uid` ends every process of the user, not only the one started (a job may
//!    detach children), and boots out the user's launchd domains so launchd restarts
//!    none of its per-user agents; `user-delete` removes the user and sweeps its home and
//!    its entry under `/var/folders`. A step that fails leaves the user in the helper's
//!    ledger, its uid held, and the lease in the journal for the next start to end. Between
//!    the two, a removal copies the run's declared outputs out of `exec/` into the run's
//!    scratch directory (`native/outputs.rs`); one that cannot be copied does not stop the
//!    removal, which then fails as [`RuntimeError`] of its outputs only.
//!
//! A start that pins a bundle (`RunSpec::bundle`) also has its bundle's trees pinned for
//! the run before anything is made, the trees the bundle writes to cloned into the run's
//! own `trees` directory, and `@bundle/<entrypoint>` resolved to the file in the store (or
//! the clone) that the bundle's config names; the environment and arguments are the
//! bundle's rules applied to the job's own (see `native/bundle.rs`). The pins end with
//! the run.
//!
//! A lease is written to the [`Journal`] (in the state root, synced) before `user-create` is
//! sent and marked done once its user is deleted or a failed start was undone. A process
//! that ends with runs going leaves them in the journal; [`Runtime::reclaim`] ends them
//! (kill, then delete) when the next worker starts and when the rescue of a dead worker
//! runs. A helper that restarted under the runtime refuses every verb until its boot reset
//! has run again; the next start, stop or removal asks for it. The helper sets the largest
//! file and the most processes the user may have on each run, so a run cannot fill the disk
//! through its output or the process table with a fork bomb.
//!
//! # Privacy grants (TCC)
//!
//! macOS's privacy controls (TCC: Full Disk Access and the like) let a process do what its
//! *responsible* process was granted, and a process answers for whoever started it unless
//! its starter disclaims that when it starts it. Nothing in this repository gives the agent
//! or the helper a grant, and neither needs one: what they touch is their own state, the
//! run users' homes and the system's tools (the helper's sweep takes the system's refusal of
//! Apple's data vaults, which root without Full Disk Access cannot examine, as an entry
//! that is not there). A grant someone gives them anyway never reaches a job. Every
//! process of a run is started through `js_mac_session::disclaim`, which disclaims the
//! responsibility, so the run's processes, and the processes they start, answer for the
//! run's program and hold no grant of the helper's or the agent's; what the run may do is
//! what its own program was granted, which this runtime neither gives nor takes away. The
//! same holds for the programs the agent starts itself (its probes, the `sandbox-exec`
//! self-test, and `pfctl` through `js_mac_provision`) and for the helper's own
//! (`launchctl`, `getconf`). `cargo test -p xtask --test spawn_sites` fails when the agent,
//! the helper or a crate built into them starts a program another way. On a Mac, the tests
//! read the responsible process of what the helper's `run` and the agent start
//! (`tests/agent/native/disclaim.rs`). The check by hand, with a grant given to the helper,
//! is in `packaging/js-agent-macos/README.md`; `docs/design/spawn-disclaim.md` has the
//! design.
//!
//! A start named by its image runs when it carries a `run` and the runtime holds a bundle
//! store: the image reference is the bundle's name (the bundle pin of the legacy form), the
//! store pulls it by digest (an index resolves to its darwin/arm64 manifest) and the `run` is
//! the native body. A start named by its image without a `run`, or on a runtime with no
//! store, is refused `image-unsupported-here`.
//!
//! # While no agent runs
//!
//! The wall limit of a native run and the silence bound (`--silence-stop`) are counted by
//! the agent's lifecycle and enforced through [`Runtime::stop`], which is `kill-uid` here: a
//! silence stops every native run the agent holds as a server's stop does, at once, with
//! no grace. The helper holds no deadline of a run's own (its `run` verb takes the program,
//! its environment and the file-size and process limits, and nothing ends the process but
//! `kill-uid`), so while the agent is down, has stopped or is restarting, a native run
//! goes on with no bound until an agent starts again: the journal then ends it (see
//! above). A deadline the helper enforces by itself is a change to the helper, not made
//! here.
//!
//! What this runtime does not do yet: carry a job start's environment (`RunSpec::env`: a
//! start with one is refused `env-unsupported` at admission, before its image or body is
//! looked at, and a run handed here with one fails before a user is made; the native body's
//! own `env`, and a `run`'s, is applied), stage inputs or run in a subdirectory (a start
//! that names inputs or a `cwd` is refused `native-unsupported`), forward a declared port (a
//! start that declares any is refused `ports-not-supported`, see `native/network.rs`), adopt
//! a run after the agent restarts (the journal ends it, and collects none of its outputs),
//! stop a run softly (the helper only kills), bound a run while no agent runs (above), or
//! grant the user's `/var/folders` entry (toolchains that need it get their temporary files
//! from `TMPDIR`, which is inside the home).
//!
//! The runtime builds on every platform so that its rules are tested on Linux; only a
//! macOS agent makes one.

mod bundle;
mod files;
mod journal;
mod logs;
mod network;
mod outputs;
mod pf;
mod settings;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt::Display;
use std::fs::{self, File};
use std::io;
use std::os::fd::AsFd as _;
use std::os::unix::fs::{PermissionsExt as _, chown};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::thread;
use std::time::Duration;

use js_agent_wire::{
    BUNDLE_PROGRAM_PREFIX, LogTail, NativeSpec, NetworkLevel, OutputSpec, refusal,
};
use js_mac_pool::seatbelt::{
    AbsPath, Caps, Narrowing, Net, Policy, Profile, ReadModel, SlotLayout,
};
use js_mac_session::client::{Client, ClientError, Exit as HelperExit};
use js_mac_session::disclaim::Program;
use js_mac_session::helper::{RESET_ALREADY, RESET_NEEDED};
use js_mac_session::lease::{Dotted, user_name};
use js_mac_session::proto::{Limits, Session};
use js_types::LeaseId;
use js_types::env::env_name_reserved;

use crate::bundle::{Pinned, Placer};
use crate::egress::NetworkRefusal;
use crate::lifecycle::{
    Body, CheckFinding, CheckHook, Exit, Finished, Found, NativeRefusal, RunHandle, RunSpec,
    Runtime, RuntimeError, Status,
};

pub use files::HandOver;
pub use journal::Journal;
pub use network::Ceiling;
pub use pf::{Enforcer, PfFlags, PfPolicy, ProvisionError};
pub use settings::{
    DEFAULT_HOMES, DEFAULT_MAX_FILE_MIB, DEFAULT_MAX_PROCESSES, DEFAULT_TOOL_ROOTS, NativeFlags,
    NativeSettings,
};

/// What a native runtime is told, from the agent's flags.
#[derive(Debug, Clone)]
pub struct NativeConfig {
    /// Where the helper makes the home of a run's user (its `--homes`), canonical: a
    /// profile compares the path the kernel resolves, and `/var` is `/private/var`.
    pub homes: PathBuf,
    /// The most network a run may ask for (`--native-network`): a start above it is refused
    /// (`network-above-ceiling`), never run with less.
    pub network: Ceiling,
    /// The directories a start's program must lie under.
    pub tool_roots: Vec<PathBuf>,
    /// Paths no run may read, such as the agent's state and scratch roots.
    pub read_denies: Vec<AbsPath>,
    /// Which reads the profile permits. The runtime builds no grants, so only
    /// [`ReadModel::Open`] gives a profile; with the other every start fails.
    pub read_model: ReadModel,
    /// The file the lease journal is kept in (in the state root).
    pub journal: PathBuf,
    /// The most bytes any one file a run writes may reach (its output included): the
    /// run's `RLIMIT_FSIZE`. A run that writes more is ended by `SIGXFSZ`.
    pub max_file_bytes: u64,
    /// The most processes the run's user may have: the run's `RLIMIT_NPROC`. A fork bomb
    /// stops at it, and the helper's kill (which forks as root) is not held by it.
    pub max_processes: u32,
    /// The check that the pf anchor of the pool is in force. Required when `network` is
    /// [`Ceiling::On`]: the runtime does not open without it. A ceiling of [`Ceiling::Open`]
    /// has none: nothing filters its jobs.
    pub egress: Option<Arc<dyn Enforcer>>,
}

/// A run that started: its scratch directory (where its log files are) and what the thread
/// waiting on the helper found.
struct Run {
    scratch: PathBuf,
    /// The home of the run's user, whose `exec/` holds the run's outputs.
    home: PathBuf,
    /// The outputs the start declared.
    outputs: Vec<OutputSpec>,
    /// What the removal copied of them, once it has: each file's path and bytes, and the
    /// paths that could not be copied (taken when the removal reports them).
    collected: Option<outputs::Collected>,
    /// Set once the run has ended, so it is answered the same way again.
    ended: Option<Status>,
    /// The exit, or `None` when the helper's connection ended without one.
    waiter: Receiver<Option<Exit>>,
    /// The trees of the run's bundle, pinned until the run is removed.
    _pinned: Option<Pinned>,
}

impl Run {
    /// Copies the run's outputs into its scratch directory, unless that was done, and logs
    /// what was copied and what was not, by path and byte count.
    fn collect(&mut self, lease: &str, max_file_bytes: u64) {
        if self.collected.is_some() {
            return;
        }
        let into = outputs::dir_in(&self.scratch);
        let caps = outputs::Caps {
            file_bytes: max_file_bytes,
            run_bytes: outputs::MAX_COLLECTED_BYTES,
            entries: outputs::MAX_ENTRIES,
        };
        let done = outputs::collect(&self.home, &self.outputs, &into, caps);
        for (path, bytes) in &done.copied {
            tracing::info!("native {lease}: output {path:?} copied, {bytes} bytes");
        }
        for why in &done.failed {
            tracing::warn!("native {lease}: output {why:?}");
        }
        self.collected = Some(done);
    }
}

/// Runs native jobs as users of the root helper.
pub struct NativeRuntime {
    helper: Client,
    config: NativeConfig,
    /// The limits of every run, checked when the runtime opens.
    limits: Limits,
    term: u64,
    next: u64,
    runs: BTreeMap<String, Run>,
    /// The bundle store, on an agent that has one.
    bundles: Option<Arc<Placer>>,
    journal: Journal,
    /// The helper said it needs its boot reset again (it restarted under this agent).
    reset_needed: bool,
    /// Gives a run's files to its user.
    hand_over: HandOver,
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
    /// A configured limit is zero or above the helper's cap (nothing is sent to the helper),
    /// the helper cannot be reached or its boot reset failed.
    pub fn open(helper: Client, config: NativeConfig, term: u64) -> Result<Self, RuntimeError> {
        if config.network == Ceiling::On && config.egress.is_none() {
            return Err(RuntimeError::new(
                "--native-network: on needs the check of the pf anchor of the pool \
                 (--native-pf-uid-start and --native-pf-uid-count)",
            ));
        }
        Self::open_ungated(helper, config, term)
    }

    /// As [`NativeRuntime::open`], without refusing a cap of `on` that has no check. For
    /// tests of the gate itself, which must refuse on its own whatever `open` let through;
    /// an agent never calls it.
    ///
    /// # Errors
    ///
    /// As [`NativeRuntime::open`], but for the missing check.
    #[doc(hidden)]
    pub fn open_ungated(
        helper: Client,
        config: NativeConfig,
        term: u64,
    ) -> Result<Self, RuntimeError> {
        let limits = limits_of(&config)?;
        reset(&helper)?;
        let journal = Journal::new(&config.journal);
        Ok(Self {
            helper,
            config,
            limits,
            term,
            next: 0,
            runs: BTreeMap::new(),
            bundles: None,
            journal,
            reset_needed: false,
            hand_over: files::lchown(),
        })
    }

    /// The same runtime, giving a run's files to its user through `hand_over` in place of
    /// `lchown`, which needs root. For tests; an agent never calls it.
    #[doc(hidden)]
    #[must_use]
    pub fn with_hand_over(self, hand_over: HandOver) -> Self {
        Self { hand_over, ..self }
    }

    /// The same runtime, with `placer` as its bundle store: a start that names a bundle
    /// pins the bundle's trees through it for the run.
    #[must_use]
    pub fn with_bundles(self, placer: Arc<Placer>) -> Self {
        Self {
            bundles: Some(placer),
            ..self
        }
    }

    /// The bundle store, if the runtime has one.
    #[must_use]
    pub fn bundles(&self) -> Option<&Arc<Placer>> {
        self.bundles.as_ref()
    }

    /// Notes that the helper restarted under this runtime: it refuses every verb until its
    /// boot reset has run again, so the next start, stop or removal asks for it first.
    fn learn(&mut self, why: &ClientError) {
        if matches!(why, ClientError::Refused(reason) if reason == RESET_NEEDED) {
            self.reset_needed = true;
        }
    }

    /// Asks for the boot reset again when the helper said it needs it.
    fn ready(&mut self) -> Result<(), RuntimeError> {
        if self.reset_needed {
            reset(&self.helper)?;
            self.reset_needed = false;
        }
        Ok(())
    }

    /// Marks `lease` done in the journal when nothing is left of it. A mark that cannot be
    /// written costs nothing but a repeated kill and delete at the next start.
    fn settle(&self, lease: &str, problems: &[String]) {
        if problems.is_empty() {
            let _ = self.journal.done(lease);
        }
    }

    /// Everything after the user exists: the run directory, the log files, the profile and
    /// the `run` verb.
    fn launch(
        &self,
        spec: &RunSpec,
        native: &NativeSpec,
        (id, lease, uid): (LeaseId, &str, u32),
        pinned: Option<Pinned>,
    ) -> Result<Run, RuntimeError> {
        let home = self.config.homes.join(user_name(id));
        let (exec, tmp) = (home.join("exec"), home.join("tmp"));
        let dirs = [&exec, &tmp]
            .into_iter()
            .map(|dir| make_dir(dir, uid).map_err(|why| fail(&dir.display(), &why)))
            .collect::<Result<Vec<File>, RuntimeError>>()?;
        // The files go to the user before any process of it exists.
        if let Some(dir) = &spec.files {
            files::hand_over(dir, uid, &self.hand_over).map_err(|why| fail(&"the files", &why))?;
        }
        let (native, mut env) = match &pinned {
            Some(pinned) => {
                let prepared = bundle::prepare(pinned, native, &home, (uid, &tmp))
                    .map_err(|why| fail(&"the bundle", &why))?;
                let body = NativeSpec {
                    program: prepared.program,
                    args: prepared.args,
                    network: prepared.network,
                    ..native.clone()
                };
                (body, prepared.env)
            }
            None => (native.clone(), plain_env(native, &tmp)),
        };
        let files = spec.files.as_deref();
        if let Some(dir) = files {
            env.push((files::FILES_DIR_ENV.to_owned(), dir.display().to_string()));
        }
        let argv = argv_of(&self.config, &native, &home, files)?;
        let logs = [logs::STDIN, logs::STDOUT, logs::STDERR]
            .into_iter()
            .map(|name| {
                let path = spec.scratch.join(name);
                open_log(&path).map_err(|why| fail(&path.display(), &why))
            })
            .collect::<Result<Vec<File>, RuntimeError>>()?;
        let fds = [
            logs[0].as_fd(),
            logs[1].as_fd(),
            logs[2].as_fd(),
            dirs[0].as_fd(),
        ];
        let running = self
            .helper
            .run_limited(
                lease,
                &argv,
                &env,
                fds,
                Session::Background,
                Some(self.limits),
            )
            .map_err(|why| fail(&"run", &why))?;
        let (sender, waiter) = channel();
        thread::spawn(move || {
            let _ = sender.send(running.wait().ok().map(exit_of));
        });
        Ok(Run {
            scratch: spec.scratch.clone(),
            home,
            outputs: native.outputs,
            collected: None,
            ended: None,
            waiter,
            _pinned: pinned,
        })
    }

    /// Pins the bundle `spec` names, if it names one, before anything of the run is made:
    /// a start whose bundle cannot be pinned, or whose program is one the bundle lacks, makes
    /// no user. The program of a start that pins no bundle may not name one.
    fn pin_bundle(
        &self,
        spec: &RunSpec,
        native: &NativeSpec,
        lease: &str,
    ) -> Result<Option<Pinned>, RuntimeError> {
        let entrypoint = native.program.strip_prefix(BUNDLE_PROGRAM_PREFIX);
        match (&spec.bundle, &self.bundles) {
            (None, _) if entrypoint.is_some() => Err(fail(
                &"the program",
                &"names an entrypoint of a bundle and the start pins none",
            )),
            (None, _) => Ok(None),
            (Some(_), None) => Err(fail(&"the bundle", &"this runtime holds no bundle store")),
            (Some(pin), Some(placer)) => bundle::pin(placer, pin, lease, entrypoint)
                .map(Some)
                .map_err(|why| fail(&"the bundle", &why)),
        }
    }

    /// `result` of the helper verb `verb` as a runtime result: a refusal that the lease has
    /// no user, or that its user was deleted, is the end that was wanted.
    fn ended(&mut self, result: Result<(), ClientError>, verb: &str) -> Result<(), RuntimeError> {
        match result {
            Err(why) if !gone(&why) => {
                self.learn(&why);
                Err(fail(&verb, &why))
            }
            _ => Ok(()),
        }
    }

    /// Whether the pf anchor of the pool is in force right now, when the ceiling is `on`
    /// (the one value whose jobs the anchor limits; an `open` ceiling has no filter to
    /// check). Asked afresh at every call: a reload of the main ruleset can drop the anchor
    /// at any time, so no answer is kept. The text is the first line of what the provisioner
    /// said, with the flag that makes the check necessary.
    fn pf_in_force(&self) -> Result<(), RuntimeError> {
        if self.config.network != Ceiling::On {
            return Ok(());
        }
        // Fails closed: `open` refuses a cap of `on` with no check, and this does not rely
        // on it.
        let Some(check) = &self.config.egress else {
            return Err(fail(
                &"--native-network",
                &"on has no check of the pf anchor of the pool",
            ));
        };
        check.enforced().map_err(|why| {
            tracing::warn!("the pf anchor is not in force: {why}");
            let said = why.to_string();
            fail(
                &"--native-network",
                &format!(
                    "on needs the pf anchor of the pool in force: {}",
                    said.lines().next().unwrap_or_default()
                ),
            )
        })
    }

    /// Ends the user of `lease` after a start that failed, so none is left behind, and says
    /// what could not be done: the start's own error is the first thing the operator needs,
    /// but a user that is left is a uid held in the helper's range.
    fn discard(&self, lease: &str) -> Vec<String> {
        let killed = self
            .helper
            .kill_uid(lease)
            .err()
            .map(|why| format!("kill-uid: {why}"));
        let deleted = self
            .helper
            .user_delete(lease)
            .err()
            .map(|why| format!("user-delete: {why}"));
        killed.into_iter().chain(deleted).collect()
    }
}

/// The environment of a run that pins no bundle: the job's own, then `TMPDIR`.
fn plain_env(native: &NativeSpec, tmp: &Path) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = native
        .env
        .iter()
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    env.push(("TMPDIR".to_owned(), tmp.display().to_string()));
    env
}

/// Asks the helper for its boot reset; one that ran already is as good as one that ran now.
fn reset(helper: &Client) -> Result<(), RuntimeError> {
    match helper.boot_reset(&[]) {
        Ok(_) => Ok(()),
        Err(ClientError::Refused(why)) if why == RESET_ALREADY => Ok(()),
        Err(why) => Err(fail(&"the boot reset", &why)),
    }
}

/// Refuses a body whose environment names a variable a job may not set
/// ([`js_types::env::env_name_reserved`]: the names that steer a shell, the agent's own
/// names, and the prefixes of the loader, the sandbox and imported shell functions). No
/// shell of the agent's reads the environment any more, but the program a job names may be
/// one. The wire checks these too; this is the last check before a user is made, for a
/// body that did not pass through it.
fn check_env(native: &NativeSpec) -> Result<(), RuntimeError> {
    match native.env.keys().find(|name| env_name_reserved(name)) {
        Some(name) => Err(RuntimeError::new(format!(
            "native.env: {name} is a reserved name"
        ))),
        None => Ok(()),
    }
}

/// Whether the helper refused because the lease has no user or its user was deleted.
fn gone(why: &ClientError) -> bool {
    matches!(why, ClientError::Refused(reason)
        if reason.contains("has no user") || reason.contains("user was deleted"))
}

/// `error`, and what could not be undone after it.
fn with_problems(error: RuntimeError, problems: &[String]) -> RuntimeError {
    match problems.is_empty() {
        true => error,
        false => RuntimeError::new(format!(
            "{}; and the lease user could not be removed ({}): the lease journal ends it at the next start",
            error.0,
            problems.join("; ")
        )),
    }
}

/// Whether the helper refused `user-create` because the lease was used before: that lease
/// is not ours to remove.
fn used_before(why: &ClientError) -> bool {
    matches!(why, ClientError::Refused(reason) if reason.contains("was used before"))
}

impl Runtime for NativeRuntime {
    fn create_and_start(&mut self, spec: &RunSpec) -> Result<RunHandle, RuntimeError> {
        let Body::Native(native) = &spec.body else {
            return Err(RuntimeError::new("this runtime runs native bodies only"));
        };
        if !spec.env.is_empty() {
            return Err(RuntimeError::new(
                "this runtime does not carry a job start's environment",
            ));
        }
        check_env(native)?;
        self.ready()?;
        let id = LeaseId::new(self.term, self.next);
        self.next += 1;
        let lease = Dotted(id).to_string();
        // A start that cannot have its bundle makes no user at all.
        let pinned = self.pin_bundle(spec, native, &lease)?;
        // Written and synced before the helper hears of the lease, so a process that dies
        // between the two leaves a lease to end and not a user nobody knows of.
        self.journal
            .add(&lease)
            .map_err(|why| fail(&"the lease journal", &why))?;
        // The helper records the lease, and the uid it holds, before it makes the home and
        // the account, so a refusal may leave a live lease: it is removed again.
        let uid = match self.helper.user_create(&lease, None) {
            Ok(uid) => uid,
            Err(why) => {
                self.learn(&why);
                let problems = match used_before(&why) {
                    true => Vec::new(),
                    false => self.discard(&lease),
                };
                self.settle(&lease, &problems);
                return Err(with_problems(fail(&"user-create", &why), &problems));
            }
        };
        match self.launch(spec, native, (id, &lease, uid), pinned) {
            Ok(run) => {
                self.runs.insert(lease.clone(), run);
                Ok(RunHandle(lease))
            }
            Err(why) => {
                let problems = self.discard(&lease);
                self.settle(&lease, &problems);
                Err(with_problems(why, &problems))
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
        self.ready()?;
        let killed = self.helper.kill_uid(&id.0);
        self.ended(killed, "kill-uid")
    }

    /// Kills, copies the run's outputs (once: a removal that is tried again does not copy
    /// them again), deletes the user, and only then reports outputs that could not be copied.
    fn remove(&mut self, id: &RunHandle) -> Result<(), RuntimeError> {
        self.ready()?;
        let killed = self.helper.kill_uid(&id.0);
        self.ended(killed, "kill-uid")?;
        let max_file_bytes = self.config.max_file_bytes;
        if let Some(run) = self.runs.get_mut(&id.0) {
            run.collect(&id.0, max_file_bytes);
        }
        let deleted = self.helper.user_delete(&id.0).map(|_| ());
        self.ended(deleted, "user-delete")?;
        let run = self.runs.remove(&id.0);
        let _ = self.journal.done(&id.0);
        let failed = run.and_then(|run| run.collected).map(|done| done.failed);
        match failed.unwrap_or_default() {
            failed if failed.is_empty() => Ok(()),
            failed => Err(RuntimeError::outputs(format!(
                "outputs not copied: {}",
                outputs::told(&failed)
            ))),
        }
    }

    fn reclaim(&mut self) -> Vec<String> {
        let live = match self.journal.live() {
            Ok(live) => live,
            Err(why) => return vec![format!("native: cannot read the lease journal: {why}")],
        };
        let mut lines = Vec::new();
        let mut retired = Vec::new();
        for lease in live {
            let killed = self.helper.kill_uid(&lease);
            let ended = self.ended(killed, "kill-uid").and_then(|()| {
                let deleted = self.helper.user_delete(&lease).map(|_| ());
                self.ended(deleted, "user-delete")
            });
            match ended {
                Ok(()) => {
                    lines.push(format!(
                        "ended the lease user {lease} an earlier process left"
                    ));
                    retired.push(lease);
                }
                Err(why) => {
                    lines.push(format!("cannot end the lease user {lease}: {}", why.0));
                }
            }
        }
        if let Err(why) = self.journal.retire(&retired) {
            lines.push(format!("native: cannot rewrite the lease journal: {why}"));
        }
        lines
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

    /// At the agent's start: with a ceiling of `on`, the pf anchor must be in force.
    fn check_egress(&self) -> Result<(), RuntimeError> {
        self.pf_in_force()
    }

    /// Before every native start, after [`Runtime::check_native`] admitted its level: a job
    /// that asks for the filtered network (`on`, the job level `allow-list`) on an agent
    /// whose ceiling is `on` is refused unless the pf anchor is in force now. The check
    /// follows the job: a job with no network, loopback only or `open` never asks pf and never
    /// fails for a missing anchor. Under a ceiling of `open` an `on` job is not checked
    /// either: that operator set no filter, which only the ceiling `on` asks for. A job whose
    /// bundle lowers the network after this is still checked, which only refuses more.
    fn check_native_egress(&self, native: &NativeSpec) -> Result<(), RuntimeError> {
        match network::needs_anchor(network::of_body(native.network)) {
            true => self.pf_in_force(),
            false => Ok(()),
        }
    }

    /// A start named by its image that reaches here is one this runtime cannot run: a
    /// bundle has no command of its own, so a start without a `run` is refused, and so is
    /// any image start on a runtime with no bundle store. It is refused before anything is
    /// made, with the code that says so, never as a native start it is not. (A start with a
    /// `run` on a runtime with a store never asks: [`Runtime::runs_image_runs`].)
    fn check_image(&self) -> Result<(), NativeRefusal> {
        Err(NativeRefusal {
            code: refusal::IMAGE_UNSUPPORTED_HERE,
            why: "this agent runs an image only as a bundle, with a run".to_owned(),
        })
    }

    /// A job start's environment would be set in the native process, which this runtime
    /// does not do yet: a start with one is refused before anything is made, never run
    /// without it.
    fn check_env(&self, env: &[(String, String)]) -> Result<(), NativeRefusal> {
        match env.is_empty() {
            true => Ok(()),
            false => Err(NativeRefusal {
                code: refusal::ENV_UNSUPPORTED,
                why: "this agent does not carry a job start's environment".to_owned(),
            }),
        }
    }

    /// With a bundle store, the image of a start with a `run` is the bundle it runs from.
    fn runs_image_runs(&self) -> bool {
        self.bundles.is_some()
    }

    /// Refuses, before anything is made: a level above the agent's ceiling
    /// (`network-above-ceiling`; it is never run with less), then any declared port
    /// (`ports-not-supported`: a Mac forwards none), then what this runtime cannot do yet.
    /// Outputs are collected when the run is removed; inputs and a working directory below
    /// `exec/` are not served yet.
    fn check_native(&self, native: &NativeSpec) -> Result<(), NativeRefusal> {
        let refused = |why: NetworkRefusal| NativeRefusal {
            code: why.code,
            why: why.detail,
        };
        network::admit(self.config.network, network::of_body(native.network)).map_err(refused)?;
        network::check_ports(&native.ports).map_err(refused)?;
        if !native.inputs.is_empty() || !native.cwd.is_empty() {
            return Err(NativeRefusal {
                code: refusal::NATIVE_UNSUPPORTED,
                why: "this agent stages no inputs and runs in no subdirectory yet".to_owned(),
            });
        }
        if native.program.starts_with(BUNDLE_PROGRAM_PREFIX) {
            return match self.bundles {
                Some(_) => Ok(()),
                None => Err(NativeRefusal {
                    code: refusal::BUNDLE_UNSUPPORTED,
                    why: "this agent holds no bundle store".to_owned(),
                }),
            };
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

/// The limits of a run under `config`, as the helper's request carries them: the largest
/// file in bytes and the most processes, both soft and hard in the helper's child, so the
/// run cannot raise them. A configured limit that is zero or above the helper's cap
/// ([`Limits::check`]) is an error.
fn limits_of(config: &NativeConfig) -> Result<Limits, RuntimeError> {
    Limits::new(config.max_file_bytes, config.max_processes)
        .map_err(|why| fail(&"the limits", &why))
}

/// The `sandbox-exec` command line of `native` under the policy of `config`, run as a user
/// whose home is `home`: `sandbox-exec`, the profile, the program and its arguments.
///
/// No shell is in the command line. The helper sets the run's limits in the
/// child before it executes `sandbox-exec`, and `sandbox-exec` is the first program of the
/// command, so nothing the job's environment steers runs outside the profile and the
/// environment does not enter the command line at all.
///
/// # Errors
///
/// The policy or the profile cannot be built, or `native.program` is not an absolute path.
/// The environment is not read here: [`NativeRuntime`] checks it before it makes a user.
pub fn profile_argv(
    config: &NativeConfig,
    native: &NativeSpec,
    home: &Path,
) -> Result<Vec<String>, RuntimeError> {
    argv_of(config, native, home, None)
}

/// [`profile_argv`] for a run whose files are in `files`: the profile denies writes there
/// as it does to an input. Reads are open under the only read model that has a profile
/// ([`ReadModel::Open`]), and the agent refuses at start a files root that a read deny
/// covers, so the run reads its files.
fn argv_of(
    config: &NativeConfig,
    native: &NativeSpec,
    home: &Path,
    files: Option<&Path>,
) -> Result<Vec<String>, RuntimeError> {
    let run = AbsPath::new(home).map_err(|why| fail(&home.display(), &why))?;
    let inputs = files.map(AbsPath::new).transpose();
    let layout = SlotLayout {
        run,
        uid_entry: None,
        inputs: inputs
            .map_err(|why| fail(&"the files", &why))?
            .into_iter()
            .collect(),
        read_denies: config.read_denies.clone(),
    };
    // The policy never grants more than the ceiling. That is a defence in depth only: a
    // start above the ceiling was refused by `check_native` before its user existed, so this
    // narrowing lowers nothing the agent admitted. What can still lower the level is the
    // bundle's `network_max` (`bundle::lower`), applied to `native` before this.
    let caps = Caps {
        net: config.network.net(),
        read_model: config.read_model,
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

/// Runs `/usr/bin/true` under the profile of `config` with `sandbox_exec`, as the agent:
/// whether the Mac has a `sandbox-exec` that accepts the profile this runtime builds. An
/// agent that fails it must not say `exec = sandbox-exec`, or every job it takes would
/// fail.
///
/// # Errors
///
/// What failed, for the operator: the profile cannot be built, the program cannot be run,
/// or it refused the profile.
pub fn self_test(config: &NativeConfig, sandbox_exec: &Path) -> Result<(), String> {
    let probe = NativeSpec {
        program: "/usr/bin/true".to_owned(),
        args: Vec::new(),
        env: BTreeMap::new(),
        cwd: String::new(),
        inputs: Vec::new(),
        outputs: Vec::new(),
        network: NetworkLevel::Off,
        ports: Vec::new(),
    };
    let argv = profile_argv(config, &probe, &config.homes).map_err(|why| why.0)?;
    let name = sandbox_exec.display();
    let out = Program::new(sandbox_exec, &argv[1..])
        .env_clear()
        .output()
        .map_err(|why| format!("{name}: {why}"))?;
    match out.status.success() {
        true => Ok(()),
        false => Err(format!(
            "{name} refused the profile ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )),
    }
}

/// The post-job check of a native agent. A Mac has no `/proc` for the container agent's
/// checks to read; what a run leaves behind is the helper's to find, and `user-delete`
/// refuses while any process of the user remains and sweeps what the user owns, so a run
/// that left residue fails its removal (and taints the host) before this runs.
#[derive(Debug, Clone, Copy)]
pub struct NativeChecks;

impl CheckHook for NativeChecks {
    fn run(&mut self, _: &Finished<'_>) -> Result<(), CheckFinding> {
        Ok(())
    }
}

/// An error of `what`, for the operator's log.
fn fail(what: &dyn Display, why: &dyn Display) -> RuntimeError {
    RuntimeError::new(format!("{what}: {why}"))
}

/// The network level of a start as the profile's. `on` and `open` are one profile, with no
/// network rule: the packet filter, which only `on` requires, is what tells them apart.
fn net_of(level: NetworkLevel) -> Net {
    match level {
        NetworkLevel::Off => Net::Off,
        NetworkLevel::Loopback => Net::Loopback,
        NetworkLevel::On | NetworkLevel::Open => Net::On,
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

/// Makes the directory `path`, private to `uid`, and opens it. No process of that user
/// exists yet, so nothing can race the steps.
fn make_dir(path: &Path, uid: u32) -> io::Result<File> {
    fs::create_dir(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    chown(path, Some(uid), None)?;
    File::open(path)
}

/// Opens (and empties) the file `path` for reading and writing: the job writes to it, the
/// runtime reads it back, and a standard input made of one reads as an end of file.
fn open_log(path: &Path) -> io::Result<File> {
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
}
