//! The runtime of native jobs: a program run on a Mac as a user of its own, under a
//! Seatbelt profile, through the root helper `js-mac-session`.
//!
//! The helper owns everything that needs root. One run uses five of its verbs, each on a
//! connection of its own (and `stop` and `watch`, under *Stops and deadlines* and in the
//! journal's paragraph below):
//!
//! 1. `user-create` makes the run's lease (`<term>.<sequence>`, never used twice) an account of
//!    its own, `kbf-lease-<term>-<sequence>`, with a home below the helper's homes directory,
//!    and answers its uid.
//! 2. The agent, as root and before any process of that user exists, makes the run
//!    directory below the user's home (`exec/`, the working directory, with `.home/`,
//!    `.tmp/` and a bundle's `.trees/` inside it) and gives it to the user. It makes the job's three standard streams as files in the
//!    lifecycle's scratch directory, which only the agent can enter: an empty one for
//!    standard input and one each for standard output and error.
//! 3. `run` starts `sandbox-exec -D ... -p <profile> <program> <args>` as the user, with
//!    the run's file-size and process limits as fields of the request: the helper sets
//!    them with `setrlimit` in the child, before it executes `sandbox-exec`, so no shell
//!    stands between the profile and the program. The helper starts it with
//!    the empty file as standard input, the log files as standard output and error, and
//!    `exec/` as the working directory. The profile is built by
//!    [`js_mac_pool::seatbelt`] from a policy that grants writes to the run directory
//!    `exec/` and nothing else: the account's home itself is not writable to the job (macOS
//!    guards what a job would make in `~/Library` even against root, so the sweep could
//!    not remove it). `HOME` and `CFFIXED_USER_HOME` name `exec/.home` and `TMPDIR` names
//!    `exec/.tmp`, so a tool that keeps preferences or caches writes there. The helper
//!    sets `HOME` to the account's home whatever the agent passes, so the command is
//!    `sandbox-exec ... /usr/bin/env HOME=<exec/.home> <program> <args>`: `env` runs
//!    inside the profile and executes the program, and the helper's wire is unchanged.
//!    The helper builds the process's environment around the one the agent passes. The connection stays open until the process exits: a thread of the runtime
//!    waits on it, and [`Runtime::poll`] reads what it found, so it never blocks.
//!    A start with run files (`RunSpec::files`, written by the lifecycle into the agent's
//!    `--run-files-root`) has their directory given to the user before this, `0500` with
//!    its files `0400`; the profile denies writes there and `JS_FILES_DIR` names it. Its
//!    mount inputs, which the lifecycle resolved below the agent's mount roots
//!    (`RunSpec::mounts`), are copied to their `dest` below `exec/` and given to the user
//!    the same way, and the profile denies writes there too (`native/files.rs`). The
//!    working directory is `exec/` or, with a `cwd`, the directory it names below `exec/`,
//!    made when it is not there; a `cwd` that leaves `exec/` (absolute, `..`, or a link
//!    that resolves outside) fails the start before `run` (`native/cwd.rs`).
//! 4. `kill-uid` ends every process of the user, not only the one started (a job may
//!    detach children), and boots out the user's launchd domains so launchd restarts
//!    none of its per-user agents; `user-delete` deletes the account, its home and its
//!    per-user folders, and shows it gone. Between the two, a removal copies the run's
//!    declared outputs out of `exec/` into the run's scratch directory (`native/outputs.rs`);
//!    one that cannot be copied does not stop the removal, which then fails as
//!    [`RuntimeError`] of its outputs only.
//!
//! A step of the end that fails at the helper leaves the account held: the helper refuses by
//! naming the step, the lease stays live in its ledger with its uid held, and in the journal.
//! The removal then ends the run and fails as [`RuntimeError::account_held`], carrying how
//! many accounts are held; the lifecycle goes on with the cleanup and reports it, and taints
//! the host from two held accounts. The agent repeats the end of each held account after each
//! removal that succeeds and at its start (`native/held.rs`); the helper does not give a held
//! uid out, so the range it gives from has one uid fewer for each, which is one native job
//! fewer at a time (a Mac agent has no native slot count of its own). A failure of the socket
//! is not a hold. A boot reset that left an account held returns its lease, and the agent
//! journals it.
//!
//! A run whose start carried a sink, on a runtime given [`LiveLogs`]
//! ([`NativeRuntime::with_live_logs`]), has its two log files uploaded while it runs and
//! finished after its end ([`Runtime::logs_report`] gives the totals); a run adopted after a
//! restart goes on with its upload from the progress record in its scratch directory. See
//! `native/logs.rs`.
//!
//! A start that pins a bundle (`RunSpec::bundle`) also has its bundle's trees pinned for
//! the run before anything is made, the trees the bundle writes to cloned into the run's
//! own `.trees` directory (below `exec/`), and `@bundle/<entrypoint>` resolved to the file in the store (or
//! the clone) that the bundle's config names; the environment and arguments are the
//! bundle's rules applied to the job's own (see `native/bundle.rs`). The pins end with
//! the run.
//!
//! A lease is written to the [`Journal`] (in the state root, synced) before `user-create` is
//! sent, with a run record once the helper has started its process (`native/journal.rs`),
//! and marked done once its user is deleted or a failed start was undone. A process that
//! ends with runs going leaves them in the journal. The runtime of the next process adopts
//! each one that has a run record ([`Runtime::list_labelled`], `native/adopt.rs`): the
//! helper's `watch` finds its process again, and the run is polled, stopped and removed as
//! any other (its outputs collected at the removal); one the helper no longer has is listed
//! ended. [`Runtime::reclaim`] ends (kill, then delete) the leases with no run record, when
//! the next worker starts and when the rescue of a dead worker runs. A helper that restarted
//! under the runtime refuses every verb until its boot reset has run again; the next start,
//! stop or removal asks for it. The helper sets the largest file and the most processes the
//! user may have on each run, so a run cannot fill the disk through its output or the
//! process table with a fork bomb.
//!
//! # Privacy grants (TCC)
//!
//! The helper holds Full Disk Access, which a profile grants it and no job; a Mac agent
//! without it refuses every native job, `no-full-disk-access`. See `native/grant.rs`.
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
//! an exec deny for a `no_exec` mount, and every mount root a read deny of every run. Layers,
//! copy-on-write mounts, targets outside or at `/work` and a remapped host port are refused at
//! admission; a host port at its own number is refused by the lifecycle (this runtime offers none).
//! A `cwd` at or below a mount's link leaves `exec/` and fails the start (`native/cwd.rs`).
//!
//! # Stops and deadlines
//!
//! [`Runtime::stop`] is the helper's `stop`: `SIGTERM` to the run's process group, and
//! `kill-uid` only if the process has not ended once the grace has passed, so a job may
//! flush. A start whose labels carry the job's wall limit asks the helper's `run` to hold a
//! deadline of that limit and the agent's stop grace, which the helper enforces by itself
//! (`kill-uid`): while an agent runs, its lifecycle has stopped the job by then; while the
//! agent is down, has stopped or is restarting, the run still ends at it. The silence bound
//! (`--silence-stop`) is the agent's alone. A helper from before `stop` and the deadline
//! refuses either as a request it cannot read; the runtime then kills at once for a stop and
//! starts the run without the deadline, so the agent's wall limit is the run's only bound
//! again (`native/bound.rs`).
//!
//! The environment of a run is the agent's (the body's own `env`, or the bundle's rules
//! applied to it, then `TMPDIR` and `JS_FILES_DIR`) with a job start's environment
//! (`RunSpec::env`) laid over it: the job's value wins where both name a variable. It travels
//! to the helper in the `env` of the `run` request, never on the command line, whose
//! arguments every user of the Mac can read (`native/env.rs`).
//!
//! A start that names a `tree` input, which is retired and which no server sends, is refused
//! `native-unsupported`.
//!
//! What this runtime does not do yet: stage a `stage` input (this agent keeps no stage
//! trees: a start that names one is refused `native-unsupported`), forward a declared port (a
//! start that declares any is refused `ports-not-supported`, see `native/network.rs`), or
//! grant the user's `/var/folders` entry (toolchains that need it get their temporary files
//! from `TMPDIR`, which is inside the run directory).
//!
//! The runtime builds on every platform so that its rules are tested on Linux; only a
//! macOS agent makes one.

mod adopt;
mod bound;
mod bundle;
mod cwd;
mod env;
mod files;
pub mod grant;
mod held;
mod journal;
mod logs;
mod mounts;
mod network;
mod outputs;
mod pf;
mod profile;
mod settings;
mod xcode;

use std::collections::BTreeMap;
use std::fmt::Display;
use std::fs::{self, File};
use std::io;
use std::os::fd::AsFd as _;
use std::os::unix::fs::{PermissionsExt as _, chown};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use js_agent_wire::{
    BUNDLE_PROGRAM_PREFIX, HostPort, InputRef, LogReport, LogTail, NativeSpec, OutputSpec, refusal,
};
use js_mac_pool::seatbelt::{AbsPath, ReadModel};
use js_mac_session::client::{Client, ClientError, Exit as HelperExit};
use js_mac_session::full_disk_access::FullDiskAccess;
use js_mac_session::helper::{RESET_ALREADY, RESET_NEEDED};
use js_mac_session::lease::{Dotted, user_name};
use js_mac_session::proto::Limits;
use js_types::LeaseId;

use crate::bundle::{Pinned, Placer};
use crate::egress::NetworkRefusal;
use crate::lifecycle::held_accounts::{HeldAccounts, gone};
use crate::lifecycle::{
    Body, Exit, Found, FsPrepared, JobFs, NativeRefusal, RunHandle, RunSpec, Runtime, RuntimeError,
    Status, check_run_env,
};
use crate::logs::{LiveLogs, RunLogs};

pub use crate::leak_scan::NativeChecks;
pub use files::HandOver;
pub use journal::{Journal, RunRecord};
pub use mounts::{MountRoot, roots_of};
pub use network::Ceiling;
pub use pf::{Enforcer, PfFlags, PfPolicy, ProvisionError};
pub use profile::{profile_argv, self_test};
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
    /// Paths no run may read, such as the agent's state and scratch roots, and every mount
    /// root: a run reads only the mounts it was given.
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
    /// The most processes the run's user may have: the run's `RLIMIT_NPROC`. A fork bomb
    /// stops at it, and the helper's kill (which forks as root) is not held by it.
    pub max_processes: u32,
    /// The check that the pf anchor of the pool is in force. Required when `network` is
    /// [`Ceiling::On`]: the runtime does not open without it. A ceiling of [`Ceiling::Open`]
    /// has none: nothing filters its jobs.
    pub egress: Option<Arc<dyn Enforcer>>,
    /// Whether the root helper holds Full Disk Access, as the agent asked it at start. Only
    /// [`FullDiskAccess::Granted`] runs native jobs: every start is refused
    /// `no-full-disk-access` before anything is made otherwise (`native/grant.rs`).
    pub full_disk_access: FullDiskAccess,
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
    /// The run's labels, as its start had them.
    labels: BTreeMap<String, String>,
    /// When the run started, in milliseconds since the Unix epoch.
    started_ms: u64,
    /// The trees of the run's bundle, pinned until the run is removed.
    _pinned: Option<Pinned>,
    /// Its live logs: the uploader, then the report.
    logs: RunLogs,
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
    /// The mounts of the job starts whose file system was resolved, by run id, until their
    /// start takes them (`native/mounts.rs`).
    plans: mounts::Plans,
    /// The bundle store, on an agent that has one.
    bundles: Option<Arc<Placer>>,
    xcodes: xcode::Table,
    journal: Journal,
    /// The helper said it needs its boot reset again (it restarted under this agent).
    reset_needed: bool,
    /// The leases whose account the helper did not delete (`native/held.rs`).
    held: HeldAccounts,
    /// Gives a run's files to its user.
    hand_over: HandOver,
    /// Live logs, when the runtime uploads them ([`NativeRuntime::with_live_logs`]).
    logs: Option<LiveLogs>,
    /// The wall clock, in milliseconds since the Unix epoch: when a run started, and so how
    /// long an adopted one has run.
    now_ms: Clock,
    /// The grace the lifecycle gives a run it stops, added to the deadline the helper holds.
    stop_grace: Duration,
    /// The helper refused a deadline or `stop` as a request it cannot read: it predates them,
    /// and neither is sent to it again (`native/bound.rs`).
    old_helper: AtomicBool,
}

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
        let booted = reset(&helper)?;
        let journal = Journal::new(&config.journal);
        let mut runtime = Self {
            helper,
            config,
            limits,
            term,
            next: 0,
            runs: BTreeMap::new(),
            plans: BTreeMap::new(),
            bundles: None,
            xcodes: BTreeMap::new(),
            journal,
            reset_needed: false,
            held: HeldAccounts::default(),
            hand_over: files::lchown(),
            logs: None,
            now_ms: system_clock(),
            stop_grace: Duration::ZERO,
            old_helper: AtomicBool::new(false),
        };
        runtime.take_booted(booted);
        Ok(runtime)
    }

    /// The same runtime, reading the wall clock from `now_ms`. For tests; an agent never
    /// calls it.
    #[doc(hidden)]
    #[must_use]
    pub fn with_clock(self, now_ms: Clock) -> Self {
        Self { now_ms, ..self }
    }

    /// The same runtime, with `grace` as the grace the lifecycle gives a run it stops: the
    /// deadline the helper holds for a run is its wall limit and this grace.
    #[must_use]
    pub fn with_stop_grace(self, grace: Duration) -> Self {
        Self {
            stop_grace: grace,
            ..self
        }
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
            let booted = reset(&self.helper)?;
            self.reset_needed = false;
            self.take_booted(booted);
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
        (pinned, plans, dev): (Option<Pinned>, &[mounts::MountPlan], Option<PathBuf>),
    ) -> Result<Run, RuntimeError> {
        let home = self.config.homes.join(user_name(id));
        let exec = profile::run_dir(&home);
        let (tmp, job_home) = (
            exec.join(profile::JOB_TMP_DIR),
            exec.join(profile::JOB_HOME_DIR),
        );
        for dir in [&exec, &tmp, &job_home] {
            make_dir(dir, uid).map_err(|why| fail(&dir.display(), &why))?;
        }
        // The files and the inputs go to the user before any process of it exists.
        if let Some(dir) = &spec.files {
            files::hand_over(dir, uid, &self.hand_over).map_err(|why| fail(&"the files", &why))?;
        }
        let make = |path: &Path| make_dir(path, uid).map(drop);
        files::stage_inputs(&exec, &spec.mounts, uid, (&self.hand_over, &make))
            .map_err(|why| fail(&"an input", &why))?;
        mounts::link_all(plans, &exec, uid, (&self.hand_over, &make))
            .map_err(|why| fail(&"a mount", &why))?;
        let cwd = cwd::open(&exec, &native.cwd, &make)
            .map_err(|why| RuntimeError::new(why.to_string()))?;
        let (native, mut env) = match &pinned {
            Some(pinned) => {
                let prepared = bundle::prepare(pinned, native, &exec, (uid, &tmp, dev.as_deref()))
                    .map_err(|why| fail(&"the bundle", &why))?;
                let body = NativeSpec {
                    program: prepared.program,
                    args: prepared.args,
                    network: prepared.network,
                    ..native.clone()
                };
                (body, prepared.env)
            }
            None => (native.clone(), env::plain(native, &tmp, dev.as_deref())),
        };
        env.push(env::job_home(&job_home));
        let files = spec.files.as_deref();
        if let Some(dir) = files {
            env.push((files::FILES_DIR_ENV.to_owned(), dir.display().to_string()));
        }
        // Written by no run: its files and its inputs, each named for an error.
        let read_only: Vec<(&str, PathBuf)> = files
            .map(|dir| ("the files", dir.to_path_buf()))
            .into_iter()
            .chain(
                spec.mounts
                    .iter()
                    .map(|input| ("an input", files::input_dest(&exec, input))),
            )
            .chain(mounts::read_only_links(plans, &exec))
            .collect();
        // The job start's environment last, over the agent's: its values win.
        let env = env::with_job(env, &spec.env);
        let grants = mounts::grants(plans);
        let argv = profile::argv_of(&self.config, &native, &home, (&read_only, &grants))?;
        let logs = logs::open(&spec.scratch).map_err(RuntimeError::new)?;
        let fds = [
            logs[0].as_fd(),
            logs[1].as_fd(),
            logs[2].as_fd(),
            cwd.as_fd(),
        ];
        let (running, deadline_ms) = self
            .start((spec, lease), (&argv, &env), fds)
            .map_err(|why| fail(&"run", &why))?;
        let record = RunRecord {
            pid: running.pid,
            started_ms: (self.now_ms)(),
            deadline_ms,
            labels: spec.labels.clone(),
            scratch: spec.scratch.clone(),
            home: home.clone(),
            outputs: native.outputs.clone(),
            bundle: spec.bundle.clone(),
        };
        // A record that cannot be written costs only the adoption: the next process ends
        // the run, as it did before runs were recorded.
        if let Err(why) = self.journal.started(lease, &record) {
            tracing::warn!("native {lease}: the run is not recorded for adoption: {why}");
        }
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
            labels: record.labels,
            started_ms: record.started_ms,
            _pinned: pinned,
            logs: logs::start(self.logs.as_ref(), spec.log_sink.as_ref(), &spec.scratch),
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
}

/// Asks the helper for its boot reset; one that ran already is as good as one that ran now.
/// Returns the leases whose account the reset did not show gone.
fn reset(helper: &Client) -> Result<Vec<String>, RuntimeError> {
    match helper.boot_reset(&[]) {
        Ok(booted) => Ok(booted.held),
        Err(ClientError::Refused(why)) if why == RESET_ALREADY => Ok(Vec::new()),
        Err(why) => Err(fail(&"the boot reset", &why)),
    }
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
        check_run_env(&spec.env)?;
        env::check_body(native)?;
        cwd::check(&native.cwd).map_err(|why| RuntimeError::new(why.to_string()))?;
        let dev = xcode::developer_dir(&self.xcodes, spec.xcode.as_deref())?;
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
        // The helper records the lease and the uid it chose before it makes the home and the
        // account, so a refusal may leave a live lease: it is removed again.
        let uid = match self.helper.user_create(&lease) {
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
        let plans = self.plans.remove(&spec.run_id).unwrap_or_default();
        match self.launch(spec, native, (id, &lease, uid), (pinned, &plans, dev)) {
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
            if matches!(run.ended, Some(Status::Exited(_))) {
                logs::end(&mut run.logs, &run.scratch);
            }
        }
        Ok(run.ended.unwrap_or(Status::Running))
    }

    /// The helper's `stop`: `SIGTERM` to the run's process group, `kill-uid` once `grace`
    /// has passed; `kill-uid` at once on a helper from before `stop` (`native/bound.rs`).
    fn stop(&mut self, id: &RunHandle, grace: Duration) -> Result<(), RuntimeError> {
        self.ready()?;
        self.soft_stop(id, grace)
    }

    /// Kills, copies the run's outputs (once: a removal that is tried again does not copy
    /// them again), deletes the user, and only then reports outputs that could not be copied.
    /// A helper that refuses to end the account leaves it held (`native/held.rs`): the run
    /// goes, its lease stays in the journal, and the error says how many accounts are held.
    /// After a removal that succeeds, the held accounts are ended again.
    fn remove(&mut self, id: &RunHandle) -> Result<(), RuntimeError> {
        self.ready()?;
        if let Err(stop) = self.end_run(&id.0) {
            return Err(self.stopped(&id.0, stop));
        }
        let run = self.runs.remove(&id.0);
        let _ = self.journal.done(&id.0);
        self.held.release(&id.0);
        self.retry_held();
        let failed = run.and_then(|run| run.collected).map(|done| done.failed);
        match failed.unwrap_or_default() {
            failed if failed.is_empty() => Ok(()),
            failed => Err(RuntimeError::outputs(format!(
                "outputs not copied: {}",
                outputs::told(&failed)
            ))),
        }
    }

    /// Ends the leases an earlier process left that have no run to adopt (a start that died
    /// before the helper started its process, or a journal from before run records); a lease
    /// with a run record is left for [`Runtime::list_labelled`]. One that cannot be ended is
    /// held and tried again after each removal (`native/held.rs`).
    fn reclaim(&mut self) -> Vec<String> {
        self.end_unadopted()
    }

    /// Every run this runtime holds, those an earlier process started adopted first
    /// (`native/adopt.rs`).
    fn list_labelled(&mut self) -> Result<Vec<Found>, RuntimeError> {
        self.adopt_all()?;
        Ok(self.found())
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

    /// Refuses, before anything is made: first a host whose helper does not hold Full Disk
    /// Access (`no-full-disk-access`, `native/grant.rs`), then a level above the agent's ceiling
    /// (`network-above-ceiling`; it is never run with less), then any declared port
    /// (`ports-not-supported`: a Mac forwards none), then a `stage` input
    /// (`native-unsupported`: this agent keeps no stage trees) or a retired `tree` input
    /// (`native-unsupported`: no server sends one, and nothing here attaches it), then a
    /// working directory whose text leaves `exec/` (`invalid-job`). Mount inputs are staged
    /// into `exec/` when the run starts and outputs are collected when it is removed.
    fn check_native(&self, native: &NativeSpec) -> Result<(), NativeRefusal> {
        grant::admit(self.config.full_disk_access)?;
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

/// The limits of a run under `config`, as the helper's request carries them: the largest
/// file in bytes and the most processes, both soft and hard in the helper's child, so the
/// run cannot raise them. A configured limit that is zero or above the helper's cap
/// ([`Limits::check`]) is an error.
fn limits_of(config: &NativeConfig) -> Result<Limits, RuntimeError> {
    Limits::new(config.max_file_bytes, config.max_processes)
        .map_err(|why| fail(&"the limits", &why))
}

/// An error of `what`, for the operator's log.
fn fail(what: &dyn Display, why: &dyn Display) -> RuntimeError {
    RuntimeError::new(format!("{what}: {why}"))
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
