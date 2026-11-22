//! The start of a native run, in eight steps ([`STEPS`]), each recorded with the wall clock
//! time it was done at for [`Runtime::start_steps`](crate::lifecycle::Runtime::start_steps):
//!
//! 1. `checked`: the body, its environment and the start's, the working directory and the
//!    Xcode the start names are checked, and the account pool must be open.
//! 2. `bundle-pinned`: the bundle the start names is pinned (a start that pins none has the
//!    step all the same), and the lease is written to the journal (synced), before any
//!    account is lent.
//! 3. `account-lent`: the pool lends the run an account that never ran a job, under the job
//!    name of its lease ([`accounts::job_of`]).
//! 4. `folder-made`: the run directory `<home>/exec` with `.home` and `.tmp` in it, owned by
//!    the account and private to it, made by the agent as root before any process of the
//!    account exists.
//! 5. `inputs-linked`: the run's files are given to the account, its mount inputs staged
//!    and its job mounts linked into `exec/` (`native/files.rs`, `native/mounts.rs`), its
//!    working directory made, and its bundle's writable trees cloned into `exec/.trees`.
//! 6. `profile-built`: the Seatbelt profile of the run (`native/profile.rs`).
//! 7. `supervisor-started`: `js-agent __supervise-native` is started through the
//!    [`Launcher`], with the run's environment (and nothing else), its working directory,
//!    and its three log files in the scratch directory as standard input, output and error.
//!    It drops to the account, sets the run's limits, applies the profile and starts the
//!    program, keeping the agent's responsibility.
//! 8. `job-ready`: the supervisor said the job started: its pid (and process group) and its
//!    audit session are kept with the run, and the journal records the run.
//!
//! A failure at any step after the account is lent retires the account and marks the lease
//! done, so nothing of the run is left; an account whose retirement fails is held by the pool,
//! its lease stays in the journal, and the start's error says so.

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use js_agent_wire::{BUNDLE_PROGRAM_PREFIX, Isolation, NativeSpec, NetworkLevel};
use js_mac_pool::seatbelt::AbsPath;

use super::journal::RunRecord;
use super::run::{Job, Run};
use super::{
    NativeRuntime, accounts, bundle, cwd, env, fail, files, logs, make_dir, mounts, profile, xcode,
};
use crate::bundle::Pinned;
use crate::lifecycle::{Body, RunHandle, RunSpec, Runtime, RuntimeError, Status, check_run_env};
use crate::mac_accounts::{Account, Pool};
use crate::mac_supervisor::{LaunchRequest, SuperviseSpec};

/// The names of the steps of a start, in the order they are done.
pub const STEPS: [&str; 8] = [
    "checked",
    "bundle-pinned",
    "account-lent",
    "folder-made",
    "inputs-linked",
    "profile-built",
    "supervisor-started",
    "job-ready",
];

/// The directory of a run's scratch directory the supervisor writes its records in.
const RECORDS_DIR: &str = "supervisor";

/// The directory below `--native-accounts` the self-test's run keeps its scratch in.
const SELF_TEST_DIR: &str = "self-test";

/// The most polls the self-test makes for its run to end, a pause apart.
const SELF_TEST_POLLS: u32 = 3000;

/// The pause between two polls of the self-test.
const SELF_TEST_PAUSE: Duration = Duration::from_millis(10);

/// What the steps after the account is lent are given.
struct Lent<'a> {
    spec: &'a RunSpec,
    native: &'a NativeSpec,
    lease: &'a str,
    account: &'a Account,
    pinned: Option<Pinned>,
    developer_dir: Option<PathBuf>,
    plans: Vec<mounts::MountPlan>,
}

impl NativeRuntime {
    /// Records the step `name` as done now.
    fn mark(&self, marks: &mut Vec<(&'static str, u64)>, name: &'static str) {
        marks.push((name, (self.now_ms)()));
    }

    /// Starts the run of `spec` (see the module documentation).
    pub(super) fn start(&mut self, spec: &RunSpec) -> Result<RunHandle, RuntimeError> {
        let mut marks = Vec::new();
        let Body::Native(native) = &spec.body else {
            return Err(RuntimeError::new("this runtime runs native bodies only"));
        };
        check_run_env(&spec.env)?;
        env::check_names("env", spec.env.iter().map(|(name, _)| name.as_str()))?;
        env::check_body(native)?;
        cwd::check(&native.cwd).map_err(|why| RuntimeError::new(why.to_string()))?;
        let developer_dir = xcode::developer_dir(&self.xcodes, spec.xcode.as_deref())?;
        let Some(pool) = self.pool.clone() else {
            return Err(RuntimeError::new(
                "the account pool is not open: the agent does not hold Full Disk Access",
            ));
        };
        self.mark(&mut marks, STEPS[0]);
        let lease = format!("{}.{}", self.term, self.next);
        self.next += 1;
        // A start that cannot have its bundle has no account lent at all.
        let pinned = self.pin_bundle(spec, native, &lease)?;
        // Written and synced before the pool hears of the lease, so a process that dies
        // between the two leaves a lease to drop and not an account nobody knows of (which
        // the pool's next open retires anyway).
        self.journal
            .add(&lease)
            .map_err(|why| fail(&"the lease journal", &why))?;
        self.mark(&mut marks, STEPS[1]);
        let account = match pool.lend(&accounts::job_of(&lease)) {
            Ok(lent) => lent.account,
            Err(why) => {
                let _ = self.journal.done(&lease);
                return Err(fail(&"the account pool", &why));
            }
        };
        self.mark(&mut marks, STEPS[2]);
        let plans = self.plans.remove(&spec.run_id).unwrap_or_default();
        let lent = Lent {
            spec,
            native,
            lease: &lease,
            account: &account,
            pinned,
            developer_dir,
            plans,
        };
        match self.launch(lent, &mut marks) {
            Ok(mut run) => {
                run.marks = marks;
                self.runs.insert(lease.clone(), run);
                Ok(RunHandle(lease))
            }
            Err(why) => Err(self.undo(&pool, &lease, account, why)),
        }
    }

    /// Retires `account` after the start of `lease` failed with `why`, and marks the lease
    /// done once the account is shown gone.
    fn undo(&self, pool: &Pool, lease: &str, account: Account, why: RuntimeError) -> RuntimeError {
        let held = accounts::retire(pool, account).err();
        if held.is_none() {
            let _ = self.journal.done(lease);
        }
        accounts::with_held(why, held)
    }

    /// The steps after the account is lent: the run directory, the inputs, the profile, the
    /// supervisor and the job.
    fn launch(
        &self,
        lent: Lent<'_>,
        marks: &mut Vec<(&'static str, u64)>,
    ) -> Result<Run, RuntimeError> {
        let Lent {
            spec,
            native,
            lease,
            account,
            pinned,
            developer_dir,
            plans,
        } = lent;
        let uid = account.uid;
        let home = self.config.homes.join(&account.name);
        let exec = profile::run_dir(&home);
        let (tmp, job_home) = (
            exec.join(profile::JOB_TMP_DIR),
            exec.join(profile::JOB_HOME_DIR),
        );
        for dir in [&exec, &tmp, &job_home] {
            make_dir(dir, uid, &self.hand_over).map_err(|why| fail(&dir.display(), &why))?;
        }
        self.mark(marks, STEPS[3]);
        // The files and the inputs go to the account before any process of it exists.
        if let Some(dir) = &spec.files {
            files::hand_over(dir, uid, &self.hand_over).map_err(|why| fail(&"the files", &why))?;
        }
        let make = |path: &Path| make_dir(path, uid, &self.hand_over).map(drop);
        files::stage_inputs(&exec, &spec.mounts, uid, (&self.hand_over, &make))
            .map_err(|why| fail(&"an input", &why))?;
        mounts::link_all(&plans, &exec, uid, (&self.hand_over, &make))
            .map_err(|why| fail(&"a mount", &why))?;
        let cwd = cwd::open(&exec, &native.cwd, &make)
            .map_err(|why| RuntimeError::new(why.to_string()))?;
        let dev = developer_dir.as_deref();
        let (native, base) = match &pinned {
            Some(pinned) => {
                let prepared =
                    bundle::prepare(pinned, native, &exec, (uid, &tmp, dev), &self.hand_over)
                        .map_err(|why| fail(&"the bundle", &why))?;
                let body = NativeSpec {
                    program: prepared.program,
                    args: prepared.args,
                    network: prepared.network,
                    ..native.clone()
                };
                (body, prepared.env)
            }
            None => (native.clone(), env::plain(native, &tmp, dev)),
        };
        self.mark(marks, STEPS[4]);
        let files = spec.files.as_deref();
        let mut base = base;
        if let Some(dir) = files {
            base.push((files::FILES_DIR_ENV.to_owned(), dir.display().to_string()));
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
            .chain(mounts::read_only_links(&plans, &exec))
            .collect();
        let grants = mounts::grants(&plans);
        let trees = mounts::trees_of(&plans)
            .map(|rules| rules.mount_trees())
            .transpose()
            .map_err(|why| fail(&"the trees", &why))?;
        let built = profile::build(&self.config, &native, &home, (&read_only, &grants), trees)?;
        self.mark(marks, STEPS[5]);
        // The job start's environment over the agent's (its values win), then the account's
        // names, which no job may set.
        let mut env = env::with_job(base, &spec.env);
        env.retain(|(name, _)| !env::ACCOUNT_ENV.contains(&name.as_str()));
        env.extend(env::account(&job_home, &tmp, &account.name));
        AbsPath::new(Path::new(&native.program)).map_err(|why| fail(&native.program, &why))?;
        let records = spec.scratch.join(RECORDS_DIR);
        fs::create_dir(&records).map_err(|why| fail(&records.display(), &why))?;
        fs::set_permissions(&records, fs::Permissions::from_mode(0o700))
            .map_err(|why| fail(&records.display(), &why))?;
        let [stdin, stdout, stderr] = logs::open(&spec.scratch).map_err(RuntimeError::new)?;
        let request = LaunchRequest {
            spec: SuperviseSpec {
                uid,
                gid: account.gid,
                new_session: self.new_session,
                profile: built.text().to_owned(),
                params: built.params().to_vec(),
                program: native.program.clone(),
                args: native.args.clone(),
                max_file_bytes: self.config.max_file_bytes,
                // The supervisor waits on the job as the account, so it is one of the
                // account's processes.
                max_processes: self.config.max_processes.saturating_add(1),
                record_dir: records.display().to_string(),
            },
            env: &env,
            cwd: &cwd,
            stdio: [stdin.into(), stdout.into(), stderr.into()],
        };
        let mut supervised = self
            .launcher
            .launch(request)
            .map_err(|why| fail(&"the supervisor", &why))?;
        self.mark(marks, STEPS[6]);
        let ready = supervised.wait_ready().map_err(|ended| {
            fail(
                &"the supervisor",
                &format!("ended before the job started: {ended:?}"),
            )
        })?;
        self.mark(marks, STEPS[7]);
        let record = RunRecord {
            pid: u32::try_from(ready.job_pid).unwrap_or_default(),
            started_ms: (self.now_ms)(),
            deadline_ms: None,
            labels: spec.labels.clone(),
            scratch: spec.scratch.clone(),
            home: home.clone(),
            outputs: native.outputs.clone(),
            bundle: spec.bundle.clone(),
            account: Some(account.name.clone()),
            uid: Some(uid),
        };
        // A record that cannot be written costs nothing a restart needs: the pool's open
        // retires the account whatever the journal says.
        if let Err(why) = self.journal.started(lease, &record) {
            tracing::warn!("native {lease}: the run is not recorded: {why}");
        }
        Ok(Run {
            scratch: spec.scratch.clone(),
            outputs: native.outputs,
            collected: None,
            ended: None,
            labels: record.labels,
            started_ms: record.started_ms,
            _pinned: pinned,
            logs: logs::start(self.logs.as_ref(), spec.log_sink.as_ref(), &spec.scratch),
            marks: Vec::new(),
            job: Some(Job {
                account: account.clone(),
                home,
                supervised,
                ready,
                kill_after: None,
            }),
        })
    }

    /// Pins the bundle `spec` names, if it names one, before anything of the run is made:
    /// a start whose bundle cannot be pinned, or whose program is one the bundle lacks, has
    /// no account lent. The program of a start that pins no bundle may not name one.
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

    /// Runs `/usr/bin/true` through every step of a start, its end and its removal, so an
    /// agent whose supervisor, profile or account pool cannot run a job never says hello. A
    /// runtime with no pool (the agent does not hold Full Disk Access) runs nothing: it
    /// refuses every native job anyway. The run's scratch directory is `self-test` below
    /// `--native-accounts`, which no run may read.
    ///
    /// # Errors
    ///
    /// A step failed, the program did not exit 0, or it did not end in time (it is stopped
    /// and removed).
    pub fn self_test(&mut self) -> Result<(), RuntimeError> {
        if self.pool.is_none() {
            return Ok(());
        }
        let scratch = self.config.accounts.state.join(SELF_TEST_DIR);
        fresh_dir(&scratch).map_err(|why| fail(&scratch.display(), &why))?;
        let spec = probe(scratch);
        let id = self.create_and_start(&spec)?;
        let mut status = Status::Running;
        for _ in 0..SELF_TEST_POLLS {
            status = self.poll(&id)?;
            if status != Status::Running {
                break;
            }
            std::thread::sleep(SELF_TEST_PAUSE);
        }
        if status == Status::Running {
            let _ = self.stop(&id, Duration::ZERO);
        }
        self.remove(&id)?;
        match status {
            Status::Exited(exit) if exit.code == 0 => Ok(()),
            Status::Running => Err(RuntimeError::new("/usr/bin/true did not end")),
            other => Err(RuntimeError::new(format!(
                "/usr/bin/true ended as {other:?}"
            ))),
        }
    }
}

/// Makes `dir` empty and private to the agent: what an earlier self-test left is removed.
fn fresh_dir(dir: &Path) -> std::io::Result<()> {
    match fs::remove_dir_all(dir) {
        Err(why) if why.kind() != std::io::ErrorKind::NotFound => return Err(why),
        _ => {}
    }
    fs::create_dir(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
}

/// The run of the self-test: `/usr/bin/true` with no network, no input and no output, its
/// scratch directory `scratch`.
fn probe(scratch: PathBuf) -> RunSpec {
    RunSpec {
        run_id: "self-test".to_owned(),
        body: Body::Native(NativeSpec {
            program: "/usr/bin/true".to_owned(),
            args: Vec::new(),
            env: Default::default(),
            cwd: String::new(),
            inputs: Vec::new(),
            outputs: Vec::new(),
            network: NetworkLevel::Off,
            ports: Vec::new(),
        }),
        env: Vec::new(),
        cpu_millis: 1000,
        memory_bytes: 1 << 30,
        pids: None,
        gpu_indices: Vec::new(),
        mounts: Vec::new(),
        scratch,
        files: None,
        slot: 0,
        isolation: Isolation::Contained,
        fuse: false,
        labels: Default::default(),
        bundle: None,
        network: Default::default(),
        ports: Vec::new(),
        host_ports: Vec::new(),
        log_sink: None,
        xcode: None,
    }
}
