//! The runtime of native jobs: a program run on a Mac as an account of its own, under a
//! Seatbelt profile, started by the agent's supervisor. No other program of ours takes part.
//!
//! One run, in the order its steps are done (`native/start.rs` names each):
//!
//! 1. The start is checked and its bundle pinned (`native/bundle.rs`), and its lease
//!    (`<term>.<sequence>`, never used twice) is written to the [`Journal`].
//! 2. The account pool ([`crate::mac_accounts::Pool`], opened by the runtime when the agent
//!    holds Full Disk Access, `native/accounts.rs`) lends the run an account that never ran
//!    a job. Its home is `<homes>/<name>`.
//! 3. The agent, as root and before any process of that account exists, makes the run
//!    directory below the account's home (`exec/`, the working directory, with `.home/`,
//!    `.tmp/` and a bundle's `.trees/` inside it) and gives it to the account. A start with
//!    run files (`RunSpec::files`, written by the lifecycle into the agent's
//!    `--run-files-root`) has their directory given to the account, `0500` with its files
//!    `0400`; the profile denies writes there and `JS_FILES_DIR` names it. Its mount inputs,
//!    which the lifecycle resolved below the agent's mount roots (`RunSpec::mounts`), are
//!    copied to their `dest` below `exec/` and given to the account the same way, and the
//!    profile denies writes there too (`native/files.rs`). The working directory is `exec/`
//!    or, with a `cwd`, the directory it names below `exec/`, made when it is not there; a
//!    `cwd` that leaves `exec/` (absolute, `..`, or a link that resolves outside) fails the
//!    start (`native/cwd.rs`).
//! 4. The profile is built by [`js_mac_pool::seatbelt`] from a policy that grants writes to
//!    the run directory `exec/` and to the account's own per-user temporary and cache folders
//!    (what macOS gives it as `DARWIN_USER_TEMP_DIR` and `DARWIN_USER_CACHE_DIR`, asked of the
//!    system as the account at the start), and nothing else: the account's home itself is not
//!    writable to the job (macOS guards what a job would make in `~/Library` even against
//!    root), and no other part of `/var/folders` is. The agent's roots, its secret files,
//!    `--native-accounts` and the places only the privacy controls guard
//!    (`native/fence.rs`) are denied reads (`native/profile.rs`).
//! 5. The agent starts `js-agent __supervise-native` ([`crate::mac_supervisor`]) with the
//!    run's environment and nothing else, the working directory, and the job's three standard
//!    streams as files in the run's scratch directory, which only the agent can enter: an
//!    empty one for standard input and one each for standard output and error. The supervisor
//!    keeps the agent's privacy responsibility (it is started without the disclaim), drops to
//!    the account, sets the run's largest file and the most processes the account may have
//!    (the configured number and one, for the supervisor itself), applies the profile, and
//!    starts the program, which keeps the responsibility too, so the profile is the fence.
//!    It says when the job started (its pid and audit session) and how it ended.
//! 6. The removal ends the run's processes (`native/end.rs`), copies its declared outputs out
//!    of `exec/` into its scratch directory (`native/outputs.rs`), retires its account (every
//!    process of the uid killed, its record and home deleted, its per-user folder emptied, all
//!    shown gone) and marks the lease done (`native/remove.rs`). An account the pool cannot
//!    show gone is held: the removal fails, and the lifecycle treats it as a failed removal.
//!    Outputs that could not be copied fail it only as [`RuntimeError`] of its outputs.
//!
//! A run whose start carried a sink, on a runtime given [`LiveLogs`]
//! ([`NativeRuntime::with_live_logs`]), has its two log files uploaded while it runs and
//! finished after its end ([`Runtime::logs_report`] gives the totals). See `native/logs.rs`.
//!
//! A start that pins a bundle (`RunSpec::bundle`) also has its bundle's trees pinned for
//! the run before anything is made, the trees the bundle writes to cloned into the run's
//! own `.trees` directory (below `exec/`), and `@bundle/<entrypoint>` resolved to the file in
//! the store (or the clone) that the bundle's config names; the environment and arguments are
//! the bundle's rules applied to the job's own (see `native/bundle.rs`). The pins end with
//! the run.
//!
//! A restart of the agent ends every native run: the pool's open retires every account an
//! earlier process lent, and the journal's runs are listed ended (`native/adopt.rs`).
//!
//! # Privacy grants (TCC)
//!
//! The agent holds Full Disk Access, which a profile grants it; a native job and its
//! supervisor keep the responsibility and answer for the agent. A Mac agent without the grant
//! opens no pool and refuses every native job, `no-full-disk-access`. See `native/grant.rs`.
//!
//! A start named by its image runs when it carries a `run` and the runtime holds a bundle
//! store: the image reference is the bundle's name (the bundle pin of the legacy form), the
//! store pulls it by digest (an index resolves to its darwin/arm64 manifest) and the `run` is
//! the native body. A start named by its image without a `run`, or on a runtime with no
//! store, is refused `image-unsupported-here`.
//!
//! A job start's mounts of host paths (`docs/design/job-rootfs.md` 5.1, 5.2, 5.4; `native/mounts.rs`)
//! are links `exec/<rest>` for a target `/work/<rest>`, made before the run's first process,
//! with a read grant (and a write grant when read-write) of the canonical host path in its profile,
//! an exec deny for a `no_exec` mount, and every mount root but the trees root a read deny of
//! every run (a tree's link is read through the trees' rules instead, `mounts::trees_of`). Layers,
//! copy-on-write mounts, targets outside or at `/work` and a remapped host port are refused at
//! admission; a host port at its own number is refused by the lifecycle (this runtime offers none).
//! A `cwd` at or below a mount's link leaves `exec/` and fails the start (`native/cwd.rs`).
//!
//! # Stops
//!
//! [`Runtime::stop`] sends `SIGTERM` to the job's process group, so a job may flush, and the
//! first poll after the grace ends the run's processes. The lifecycle holds the wall limit and
//! the silence bound (`--silence-stop`) and calls the stop.
//!
//! The environment of a run is the agent's (the body's own `env`, or the bundle's rules
//! applied to it, then `TMPDIR` and `JS_FILES_DIR`) with a job start's environment
//! (`RunSpec::env`) laid over it: the job's value wins where both name a variable; then the
//! account's names (`HOME`, `CFFIXED_USER_HOME`, `USER`, `LOGNAME`, `xcrun_db`), which no job
//! may set. It reaches the supervisor as its environment, never on the command line, whose
//! arguments every user of the Mac can read (`native/env.rs`).
//!
//! A start that names a `tree` input, which is retired and which no server sends, is refused
//! `native-unsupported`.
//!
//! What this runtime does not do yet: stage a `stage` input (this agent keeps no stage
//! trees: a start that names one is refused `native-unsupported`), forward a declared port (a
//! start that declares any is refused `ports-not-supported`, see `native/network.rs`).
//! Toolchains get their temporary files from `TMPDIR` (inside the run directory) and may also
//! write the account's own per-user temporary and cache folders.
//!
//! The runtime is built in the macOS build only; the decisions it makes that need no Mac
//! (the account pool, the supervisor's steps) live in modules every build tests.

mod accounts;
mod adopt;
mod bundle;
mod cwd;
mod end;
mod env;
pub mod fence;
mod files;
pub mod grant;
mod journal;
mod logs;
mod mounts;
mod network;
mod outputs;
mod pf;
mod profile;
mod remove;
mod run;
mod settings;
mod start;
mod xcode;

use std::collections::BTreeMap;
use std::fmt::Display;
use std::fs::{self, File};
use std::io;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use js_agent_wire::{
    BUNDLE_PROGRAM_PREFIX, HostPort, InputRef, LogReport, LogTail, NativeSpec, refusal,
};
use js_mac_pool::seatbelt::{AbsPath, ReadModel};
use js_mac_session::full_disk_access::FullDiskAccess;

use crate::bundle::Placer;
use crate::egress::NetworkRefusal;
use crate::lifecycle::{
    Found, FsPrepared, JobFs, NativeRefusal, RunHandle, RunSpec, Runtime, RuntimeError, Status,
};
use crate::logs::LiveLogs;
use crate::mac_accounts::{Pool, Retired};
use crate::mac_supervisor::Launcher;

pub use accounts::Accounts;
pub use files::HandOver;
pub use journal::{Journal, RunRecord};
pub use mounts::{MountRoot, roots_of};
pub use network::Ceiling;
pub use pf::{Enforcer, PfFlags, PfPolicy, ProvisionError};
pub use settings::{
    DEFAULT_HOMES, DEFAULT_MAX_FILE_MIB, DEFAULT_MAX_PROCESSES, DEFAULT_TOOL_ROOTS,
    MIN_MAX_FILE_MIB, NativeFlags, NativeSettings,
};
pub use start::STEPS;

/// What a native runtime is told, from the agent's flags.
#[derive(Debug, Clone)]
pub struct NativeConfig {
    /// The account pool the runs are lent their accounts from.
    pub accounts: Accounts,
    /// Where the pool makes its accounts' homes, canonical: a profile compares the path the
    /// kernel resolves.
    pub homes: PathBuf,
    /// The most network a run may ask for (`--native-network`): a start above it is refused
    /// (`network-above-ceiling`), never run with less.
    pub network: Ceiling,
    /// The directories a start's program must lie under.
    pub tool_roots: Vec<PathBuf>,
    /// Paths no run may read, such as the agent's state and scratch roots, every mount root
    /// (a run reads only the mounts it was given) and the places only the privacy controls
    /// guard.
    pub read_denies: Vec<AbsPath>,
    /// The roots a job's mounts must lie beneath once their links are followed
    /// (`--mount-root`, `--mount-root-ro`), canonical; also in `read_denies`.
    pub mount_roots: Vec<MountRoot>,
    /// Which reads the profile permits. The runtime builds no grants, so only
    /// [`ReadModel::Open`] gives a profile; with the other every start fails.
    pub read_model: ReadModel,
    /// The file the lease journal is kept in (in the state root).
    pub journal: PathBuf,
    /// The most bytes any one file a run writes may reach (its output included): the
    /// run's `RLIMIT_FSIZE`. A run that writes more is ended by `SIGXFSZ`.
    pub max_file_bytes: u64,
    /// The most processes a run may start. The run's `RLIMIT_NPROC` is this and one, for the
    /// supervisor that waits on the job as the run's account; a fork bomb stops at it.
    pub max_processes: u32,
    /// The check that the pf anchor of the pool is in force. Required when `network` is
    /// [`Ceiling::On`]: the runtime does not open without it. A ceiling of [`Ceiling::Open`]
    /// has none: nothing filters its jobs.
    pub egress: Option<Arc<dyn Enforcer>>,
    /// Whether the agent holds Full Disk Access, as its listing found at start. Only
    /// [`FullDiskAccess::Granted`] opens the pool and runs native jobs: every start is
    /// refused `no-full-disk-access` before anything is made otherwise (`native/grant.rs`).
    pub full_disk_access: FullDiskAccess,
}

/// Runs native jobs as accounts of the agent's pool, started by its supervisor.
pub struct NativeRuntime {
    /// The account pool; `None` when the agent does not hold Full Disk Access, and then every
    /// start is refused.
    pool: Option<Arc<Pool>>,
    /// What starts the supervisor of a run.
    launcher: Arc<dyn Launcher>,
    config: NativeConfig,
    term: u64,
    next: u64,
    runs: BTreeMap<String, run::Run>,
    /// The mounts of the job starts whose file system was resolved, by run id, until their
    /// start takes them (`native/mounts.rs`).
    plans: mounts::Plans,
    /// The bundle store, on an agent that has one.
    bundles: Option<Arc<Placer>>,
    xcodes: xcode::Table,
    journal: Journal,
    /// Gives a run's files to its account.
    hand_over: HandOver,
    /// Live logs, when the runtime uploads them ([`NativeRuntime::with_live_logs`]).
    logs: Option<LiveLogs>,
    /// The wall clock, in milliseconds since the Unix epoch: when a run started and each step
    /// of its start was done, and when a stop's grace ends.
    now_ms: Clock,
    /// Whether each run's supervisor opens an audit session of its own (which needs root).
    new_session: bool,
    /// The account the last removal retired, and what its retirement showed.
    last_retired: Option<(String, Retired)>,
    /// What finds the lent account's own per-user temporary and cache folders, which its
    /// run may write; none grants nothing there.
    own_folders: Option<OwnFolders>,
    /// How many polls the self-test makes for its run to end; none is the default
    /// ([`NativeRuntime::with_self_test_polls`] sets it for tests).
    self_test_polls: Option<u32>,
}

/// Finds an account's own per-user temporary and cache folders (its uid's, below
/// `/private/var/folders`), given the account and its home.
pub type OwnFolders =
    Arc<dyn Fn(&crate::mac_accounts::Account, &Path) -> io::Result<Vec<PathBuf>> + Send + Sync>;

/// A wall clock: milliseconds since the Unix epoch.
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// The system's wall clock.
fn system_clock() -> Clock {
    Arc::new(|| {
        let since = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
    })
}

impl NativeRuntime {
    /// A runtime whose runs start through `launcher`, over the account pool of
    /// `config.accounts` on this Mac, which it opens when `config.full_disk_access` is
    /// [`FullDiskAccess::Granted`] (and leaves closed otherwise: every start is then refused).
    /// The pool is opened with no adopted job, so every account an earlier process lent is
    /// retired. Its run ids are `term.0`, `term.1` and so on.
    ///
    /// # Errors
    ///
    /// A ceiling of `on` with no check of the pf anchor, or a pool that cannot be opened.
    pub fn open(
        launcher: Arc<dyn Launcher>,
        config: NativeConfig,
        term: u64,
    ) -> Result<Self, RuntimeError> {
        gate(&config)?;
        let pool = accounts::open(&config.accounts, config.full_disk_access)?;
        let own: OwnFolders = Arc::new(crate::mac_accounts::own_folders);
        Ok(Self::open_ungated(pool, launcher, config, term)?.with_own_folders(own))
    }

    /// As [`NativeRuntime::open`], over `pool` (opened by the caller, over a host of its
    /// own): how the tests give a pool over a fake host. An agent never calls it.
    ///
    /// # Errors
    ///
    /// A ceiling of `on` with no check of the pf anchor.
    #[doc(hidden)]
    pub fn over(
        pool: Option<Arc<Pool>>,
        launcher: Arc<dyn Launcher>,
        config: NativeConfig,
        term: u64,
    ) -> Result<Self, RuntimeError> {
        gate(&config)?;
        Self::open_ungated(pool, launcher, config, term)
    }

    /// As [`NativeRuntime::over`], without refusing a cap of `on` that has no check. For
    /// tests of the gate itself, which must refuse on its own whatever `open` let through;
    /// an agent never calls it.
    ///
    /// # Errors
    ///
    /// None today; the signature is that of the other constructors.
    #[doc(hidden)]
    pub fn open_ungated(
        pool: Option<Arc<Pool>>,
        launcher: Arc<dyn Launcher>,
        config: NativeConfig,
        term: u64,
    ) -> Result<Self, RuntimeError> {
        let journal = Journal::new(&config.journal);
        Ok(Self {
            pool,
            launcher,
            config,
            term,
            next: 0,
            runs: BTreeMap::new(),
            plans: BTreeMap::new(),
            bundles: None,
            xcodes: BTreeMap::new(),
            journal,
            hand_over: files::lchown(),
            logs: None,
            now_ms: system_clock(),
            new_session: true,
            last_retired: None,
            own_folders: None,
            self_test_polls: None,
        })
    }

    /// The account the last removal retired and what its retirement showed (the processes it
    /// killed, each item of the proof that it is gone): what the development verb prints.
    #[doc(hidden)]
    #[must_use]
    pub fn last_retired(&self) -> Option<&(String, Retired)> {
        self.last_retired.as_ref()
    }

    /// Whether the runtime has an account pool: the agent holds Full Disk Access.
    #[must_use]
    pub fn has_pool(&self) -> bool {
        self.pool.is_some()
    }

    /// The same runtime, reading the wall clock from `now_ms`. For tests; an agent never
    /// calls it.
    #[doc(hidden)]
    #[must_use]
    pub fn with_clock(self, now_ms: Clock) -> Self {
        Self { now_ms, ..self }
    }

    /// The same runtime, finding each lent account's own per-user temporary and cache folders
    /// with `find`, which the run may then write (as [`NativeRuntime::open`] does with the
    /// system's answer, asked as the account). A runtime made otherwise grants none.
    #[must_use]
    pub fn with_own_folders(self, find: OwnFolders) -> Self {
        Self {
            own_folders: Some(find),
            ..self
        }
    }

    /// Whether the runtime finds the lent account's own per-user folders
    /// ([`NativeRuntime::with_own_folders`]): [`NativeRuntime::open`] sets it, the other
    /// constructors do not. For tests; an agent never calls it.
    #[doc(hidden)]
    #[must_use]
    pub fn has_own_folders(&self) -> bool {
        self.own_folders.is_some()
    }

    /// The same runtime, its self-test making at most `polls` polls for its run to end (the
    /// default when never called): with none, the run is still going when the count is
    /// reached, so the self-test stops and removes it. For tests; an agent never calls it.
    #[doc(hidden)]
    #[must_use]
    pub fn with_self_test_polls(self, polls: u32) -> Self {
        Self {
            self_test_polls: Some(polls),
            ..self
        }
    }

    /// The same runtime, its supervisors opening no audit session of their own, which only root
    /// may: how a test runs the real supervisor as the user running the tests. An agent never
    /// calls it.
    #[doc(hidden)]
    #[must_use]
    pub fn without_new_session(self) -> Self {
        Self {
            new_session: false,
            ..self
        }
    }

    /// The same runtime, giving a run's files to its account through `hand_over` in place of
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

    /// The same runtime, uploading with `logs` the output of each run whose start carries a
    /// sink (`native/logs.rs`).
    #[must_use]
    pub fn with_live_logs(self, logs: LiveLogs) -> Self {
        let logs = Some(logs);
        Self { logs, ..self }
    }

    /// The bundle store, if the runtime has one.
    #[must_use]
    pub fn bundles(&self) -> Option<&Arc<Placer>> {
        self.bundles.as_ref()
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
}

/// Refuses a ceiling of `on` with no check of the pf anchor.
fn gate(config: &NativeConfig) -> Result<(), RuntimeError> {
    match config.network == Ceiling::On && config.egress.is_none() {
        true => Err(RuntimeError::new(
            "--native-network: on needs the check of the pf anchor of the pool \
             (--native-pf-uid-start and --native-pf-uid-count)",
        )),
        false => Ok(()),
    }
}

impl Runtime for NativeRuntime {
    fn create_and_start(&mut self, spec: &RunSpec) -> Result<RunHandle, RuntimeError> {
        self.start(spec)
    }

    /// The steps of the start of `id` ([`STEPS`]), each with when it was done; once.
    fn start_steps(&mut self, id: &RunHandle) -> Vec<(&'static str, u64)> {
        let run = self.runs.get_mut(&id.0);
        run.map(|run| std::mem::take(&mut run.marks))
            .unwrap_or_default()
    }

    fn poll(&mut self, id: &RunHandle) -> Result<Status, RuntimeError> {
        Ok(self.poll_run(id))
    }

    /// `SIGTERM` to the job's process group; its processes are ended at the first poll after
    /// `grace` (`native/remove.rs`).
    fn stop(&mut self, id: &RunHandle, grace: Duration) -> Result<(), RuntimeError> {
        self.stop_run(id, grace);
        Ok(())
    }

    /// Ends the run's processes, copies its outputs, retires its account, and only then
    /// reports outputs that could not be copied (`native/remove.rs`).
    fn remove(&mut self, id: &RunHandle) -> Result<(), RuntimeError> {
        self.remove_run(id)
    }

    /// Drops the leases an earlier process left whose job never started (`native/adopt.rs`).
    fn reclaim(&mut self) -> Vec<String> {
        self.drop_unstarted()
    }

    /// Every run this runtime holds, the runs an earlier process left listed ended first
    /// (`native/adopt.rs`).
    fn list_labelled(&mut self) -> Result<Vec<Found>, RuntimeError> {
        self.list_ended()?;
        Ok(self.found())
    }

    /// Refuses a job start's environment that sets a name of the run's account
    /// (`invalid-env`): the runtime sets those itself (`native/env.rs`).
    fn check_env(&self, env: &[(String, String)]) -> Result<(), NativeRefusal> {
        env::check_names("env", env.iter().map(|(name, _)| name.as_str())).map_err(|why| {
            NativeRefusal {
                code: refusal::INVALID_ENV,
                why: why.0,
            }
        })
    }

    fn log_tail(&mut self, id: &RunHandle) -> Result<Option<LogTail>, RuntimeError> {
        match self.runs.get(&id.0) {
            None => Ok(None),
            Some(run) => logs::tail(&run.scratch)
                .map(Some)
                .map_err(|why| fail(&"the log", &why)),
        }
    }

    fn logs_report(&mut self, id: &RunHandle) -> Option<LogReport> {
        self.runs.get(&id.0).and_then(|run| run.logs.report())
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

    /// Mounts of host paths, as links in `exec/` (`native/mounts.rs`); layers are refused.
    fn layered_rootfs(&self) -> bool {
        true
    }

    fn check_job_fs(&self, fs: &JobFs, host_ports: &[HostPort]) -> Result<(), NativeRefusal> {
        mounts::check(fs, host_ports)
    }

    fn resolve_fs(&mut self, run_id: &str, _image: &str, fs: &JobFs, _now_ms: u64) -> FsPrepared {
        mounts::resolve(&self.config.mount_roots, &mut self.plans, run_id, fs)
    }

    fn discard_fs(&mut self, run_id: &str) -> Result<(), RuntimeError> {
        self.plans.remove(run_id);
        Ok(())
    }

    /// With a bundle store, the image of a start with a `run` is the bundle it runs from.
    fn runs_image_runs(&self) -> bool {
        self.bundles.is_some()
    }

    fn check_xcode(&self, build: &str) -> Result<(), NativeRefusal> {
        xcode::check(&self.xcodes, build)
    }

    /// Refuses, before anything is made: first a host whose agent does not hold Full Disk
    /// Access (`no-full-disk-access`, `native/grant.rs`), then a body whose environment sets
    /// a name of the run's account (`invalid-env`), then a level above the agent's ceiling
    /// (`network-above-ceiling`; it is never run with less), then any declared port
    /// (`ports-not-supported`: a Mac forwards none), then a `stage` input
    /// (`native-unsupported`: this agent keeps no stage trees) or a retired `tree` input
    /// (`native-unsupported`: no server sends one, and nothing here attaches it), then a
    /// working directory whose text leaves `exec/` (`invalid-job`). Mount inputs are staged
    /// into `exec/` when the run starts and outputs are collected when it is removed.
    fn check_native(&self, native: &NativeSpec) -> Result<(), NativeRefusal> {
        grant::admit(self.config.full_disk_access)?;
        env::check_names("native.env", native.env.keys().map(String::as_str)).map_err(|why| {
            NativeRefusal {
                code: refusal::INVALID_ENV,
                why: why.0,
            }
        })?;
        let refused = |why: NetworkRefusal| NativeRefusal {
            code: why.code,
            why: why.detail,
        };
        network::admit(self.config.network, network::of_body(native.network)).map_err(refused)?;
        network::check_ports(&native.ports).map_err(refused)?;
        for input in &native.inputs {
            let why = match input {
                InputRef::Mount { .. } => continue,
                InputRef::Stage { .. } => {
                    "this agent keeps no stage trees: only mount inputs are staged"
                }
                InputRef::Tree { .. } => "a tree input is retired: only mount inputs are staged",
            };
            return Err(NativeRefusal {
                code: refusal::NATIVE_UNSUPPORTED,
                why: why.to_owned(),
            });
        }
        cwd::check(&native.cwd).map_err(|why| NativeRefusal {
            code: refusal::INVALID_JOB,
            why: why.to_string(),
        })?;
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

/// An error of `what`, for the operator's log.
fn fail(what: &dyn Display, why: &dyn Display) -> RuntimeError {
    RuntimeError::new(format!("{what}: {why}"))
}

/// Makes the directory `path`, private to `uid`, given to it through `hand_over` (its group
/// unchanged), and opens it. No process of that account exists yet, so nothing can race the
/// steps.
fn make_dir(path: &Path, uid: u32, hand_over: &HandOver) -> io::Result<File> {
    fs::create_dir(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    let gid = fs::symlink_metadata(path)?.gid();
    hand_over(path, uid, gid)?;
    File::open(path)
}
