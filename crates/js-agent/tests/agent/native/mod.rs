//! Native jobs: the runtime that lends each run an account of the agent's pool and starts it
//! through the supervisor, under a Seatbelt profile, and the lifecycle that drives it. The
//! pool is the real one over a fake of the operating system (`fake_accounts`), so the pool's
//! ledger and its file work run as on a Mac; the supervisor is a fake that starts the program
//! itself as the test's own user (`fake_launcher`), so the runtime's request to it (the
//! account, the profile, the environment, the limits) is read back by the tests, and the
//! program's output and exit are real. One module reads back that what the agent starts
//! answers to the privacy controls for itself (`disclaim`).

mod adopt;
pub(crate) mod agent;
mod agent_start;
mod bundle;
#[cfg(target_os = "macos")]
mod disclaim;
pub(crate) mod end;
mod ended;
mod env;
pub(crate) mod fake_accounts;
pub(crate) mod fake_launcher;
mod files;
mod grant;
mod inputs;
mod job_home;
mod journal;
mod lifecycle;
mod live_logs;
#[cfg(target_os = "macos")]
mod mac;
#[cfg(target_os = "macos")]
mod mac_pf;
mod mounts;
mod network;
mod outputs;
mod pf_gate;
mod put;
mod runtime;
mod settings;
mod silence;
mod steps;
mod usage;
mod worker;
mod xcode;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use js_agent::lifecycle::{Body, RunHandle, RunSpec, Runtime, Status};
use js_agent::mac_accounts::{Places, Pool, Settings, Slots};
use js_agent::mac_usage::Series;
use js_agent::native::{Accounts, Ceiling, HandOver, NativeConfig, NativeRuntime};
use js_agent_wire::{Isolation, NativeSpec, NetworkLevel};
use js_mac_pool::seatbelt::{AbsPath, ReadModel};
use js_mac_session::full_disk_access::FullDiskAccess;

pub use crate::support::Scratch;
pub use fake_accounts::FakeAccounts;
pub use fake_launcher::{Failing, FakeLauncher, Launched};

/// The term of the runtimes made here: their leases are `7.0`, `7.1` and so on, lent under
/// the job names `7-0`, `7-1`.
pub const TERM: u64 = 7;

/// The most polls a test makes for a run to end. The program runs as a real process, so a
/// poll right after its start may still say it goes on; this bounds the loop instead of a
/// clock.
pub const POLLS: u32 = 50_000_000;

/// This process's own uid: the first slot of every pool made here, so the directories the
/// runtime gives a run's account are the test's own.
pub fn me() -> u32 {
    rustix::process::geteuid().as_raw()
}

/// This process's own group.
pub fn my_gid() -> u32 {
    rustix::process::getegid().as_raw()
}

/// The pool's directories below `dir`, laid out as a Mac's are: the state directory (the
/// owner's alone to write in), the homes, the per-user folders and the places a sweep covers.
/// `slots` uids from the test's own.
pub fn pool_settings(dir: &Scratch, slots: u32) -> Settings {
    for sub in [
        "accounts", "Users", "folders", "at/tabs", "at/jobs", "Shared", "tmp", "launchd",
    ] {
        fs::create_dir_all(dir.path(sub)).unwrap();
    }
    fs::set_permissions(dir.path("accounts"), fs::Permissions::from_mode(0o755)).unwrap();
    Settings {
        range: me()..=me() + slots - 1,
        gid: my_gid(),
        homes: dir.path("Users"),
        ready: 1,
        user_folders: dir.path("folders"),
        schedules: Places {
            named: vec![format!("{}/{{name}}", dir.path("at/tabs").display())],
            owned: vec![dir.path("at/jobs")],
        },
        shared: Places {
            named: vec![format!(
                "{}/disabled.{{uid}}.plist",
                dir.path("launchd").display()
            )],
            owned: vec![dir.path("Shared"), dir.path("tmp")],
        },
        state_owner: Some(me()),
    }
}

/// The pool of `settings` in `dir`'s `accounts` directory, over `host`, with no adopted job.
pub fn open_pool(dir: &Scratch, settings: &Settings, host: &Arc<FakeAccounts>) -> Arc<Pool> {
    open_pool_adopting(dir, settings, host, &BTreeSet::new())
}

/// The pool of `settings` in `dir`'s `accounts` directory, over `host`, keeping the accounts of
/// the jobs `adopted`.
pub fn open_pool_adopting(
    dir: &Scratch,
    settings: &Settings,
    host: &Arc<FakeAccounts>,
    adopted: &BTreeSet<String>,
) -> Arc<Pool> {
    let pool = Pool::with_settings(
        &dir.path("accounts"),
        settings.clone(),
        adopted,
        host.clone(),
    );
    Arc::new(pool.unwrap_or_else(|why| panic!("the pool: {why}")))
}

/// The pool of `settings` in `dir`'s `accounts` directory, over `host`, opened as an agent's
/// next process opens it: keeping the accounts of the runs of `dir`'s journal it takes back.
pub fn reopen_pool(dir: &Scratch, settings: &Settings, host: &Arc<FakeAccounts>) -> Arc<Pool> {
    let adopted = NativeRuntime::adoptable(&dir.path("native-leases"));
    open_pool_adopting(dir, settings, host, &adopted)
}

/// The configuration of the runtimes made here, in `dir`: a loopback cap, the tool roots
/// `/usr/bin` and `/bin`, one path no run may read, the pool's homes, and Full Disk Access
/// held.
pub fn config(dir: &Scratch, settings: &Settings) -> NativeConfig {
    let first = *settings.range.start();
    let last = *settings.range.end();
    NativeConfig {
        accounts: Accounts {
            state: dir.path("accounts"),
            slots: format!("{first}-{last}").parse::<Slots>().unwrap(),
            ready: settings.ready,
        },
        homes: settings.homes.clone(),
        network: Ceiling::Loopback,
        tool_roots: vec![PathBuf::from("/usr/bin"), PathBuf::from("/bin")],
        read_denies: vec![AbsPath::parse("/srv/js-state").unwrap()],
        mount_roots: Vec::new(),
        host_ports: Vec::new(),
        read_model: ReadModel::Open,
        journal: dir.path("native-leases"),
        max_file_bytes: 1 << 30,
        max_processes: 512,
        egress: None,
        full_disk_access: FullDiskAccess::Granted,
    }
}

/// A hand-over that gives nothing away: the test's user cannot give a directory to a uid of
/// its pool other than its own.
pub fn keep_owner() -> HandOver {
    Arc::new(|_: &Path, _: u32, _: u32| Ok(()))
}

/// A native runtime over a pool of a fake host and a fake launcher, with the directory they
/// work in.
pub struct Rig {
    pub accounts: Arc<FakeAccounts>,
    pub launcher: Arc<FakeLauncher>,
    pub pool: Arc<Pool>,
    pub settings: Settings,
    pub rt: NativeRuntime,
    pub dir: Scratch,
}

impl Rig {
    /// A rig of one slot (the test's own uid).
    pub fn new(name: &str) -> Self {
        Self::with(name, |_| {})
    }

    /// A rig whose configuration `adjust` has changed.
    pub fn with(name: &str, adjust: impl FnOnce(&mut NativeConfig)) -> Self {
        Self::in_dir(name, 1, |config, _| adjust(config))
    }

    /// A rig of `slots` slots whose configuration `adjust` has changed, knowing the
    /// directory the rig works in (the runs' scratch directories are made below `scratch`
    /// there). A rig of more than one slot gives nothing away ([`keep_owner`]).
    pub fn in_dir(
        name: &str,
        slots: u32,
        adjust: impl FnOnce(&mut NativeConfig, &Scratch),
    ) -> Self {
        let dir = Scratch::new(&format!("native-{name}"));
        let accounts = FakeAccounts::new();
        let launcher = FakeLauncher::new();
        let settings = pool_settings(&dir, slots);
        let pool = open_pool(&dir, &settings, &accounts);
        let mut config = config(&dir, &settings);
        adjust(&mut config, &dir);
        // The runs are the test user's own processes: their usage is sampled from a fake
        // that finds none, so no test reads (or waits on) every process of the user.
        let rt = NativeRuntime::over(Some(pool.clone()), launcher.clone(), config, TERM)
            .unwrap()
            .with_process_table(end::GroupTable::new(launcher.clone()))
            .with_process_usage(Arc::new(usage::Fixed::none()));
        let rt = match slots {
            1 => rt,
            _ => rt.with_hand_over(keep_owner()),
        };
        Self {
            accounts,
            launcher,
            pool,
            settings,
            rt,
            dir,
        }
    }

    /// The same rig, its runtime giving a run's directories and files to the run's account
    /// through `hand_over`.
    pub fn handing_over(self, hand_over: HandOver) -> Self {
        let rt = self.rt.with_hand_over(hand_over);
        Self { rt, ..self }
    }

    /// The same rig, its runtime uploading the live logs of a run with a sink with `logs`.
    pub fn living(self, logs: js_agent::logs::LiveLogs) -> Self {
        let rt = self.rt.with_live_logs(logs);
        Self { rt, ..self }
    }

    /// The same rig, its runtime sampling its runs' processes from `processes` and recording
    /// the series `series` names.
    pub fn sampling(self, processes: Arc<usage::Fixed>, series: Series) -> Self {
        let rt = self.rt.with_process_usage(processes).with_series(series);
        Self { rt, ..self }
    }

    /// The same rig, its runtime holding `xcodes`: a start may name the build of one.
    pub fn holding(self, xcodes: Vec<js_agent::xcode::Xcode>) -> Self {
        let rt = self.rt.with_xcodes(xcodes);
        Self { rt, ..self }
    }

    /// A second runtime over the same state, as an agent's next process opens it: the pool
    /// is opened again over the same host keeping the accounts of the runs the journal's
    /// supervisors let it take back ([`NativeRuntime::adoptable`]), the journal is the same.
    pub fn reopened(&self) -> NativeRuntime {
        self.reopened_with_pool().0
    }

    /// [`Rig::reopened`], with the pool the second runtime opened.
    pub fn reopened_with_pool(&self) -> (NativeRuntime, Arc<Pool>) {
        let pool = reopen_pool(&self.dir, &self.settings, &self.accounts);
        let config = config(&self.dir, &self.settings);
        let rt = NativeRuntime::over(Some(pool.clone()), self.launcher.clone(), config, TERM + 1)
            .unwrap()
            .with_process_table(end::GroupTable::new(self.launcher.clone()))
            .with_process_usage(Arc::new(usage::Fixed::none()));
        (rt, pool)
    }

    /// A run of `native`, with its scratch directory made as the lifecycle makes it.
    pub fn spec(&self, run_id: &str, native: NativeSpec) -> RunSpec {
        spec_in(&self.dir, run_id, native)
    }

    /// Starts `program args` and returns its handle.
    pub fn start(&mut self, run_id: &str, program: &str, args: &[&str]) -> RunHandle {
        let spec = self.spec(run_id, native(program, args));
        self.rt.create_and_start(&spec).unwrap()
    }

    /// Polls until the run is no longer running.
    pub fn settle(&mut self, id: &RunHandle) -> Status {
        settle(&mut self.rt, id)
    }

    /// The home of the account named `name`.
    pub fn home_of(&self, name: &str) -> PathBuf {
        self.settings.homes.join(name)
    }
}

/// Polls `rt` until the run `id` is no longer running.
pub fn settle(rt: &mut NativeRuntime, id: &RunHandle) -> Status {
    for _ in 0..POLLS {
        let status = rt.poll(id).unwrap();
        if status != Status::Running {
            return status;
        }
        std::thread::yield_now();
    }
    panic!("the run did not end");
}

/// A run of `native` whose scratch directory is made below `dir`, as the lifecycle makes it.
pub fn spec_in(dir: &Scratch, run_id: &str, native: NativeSpec) -> RunSpec {
    let scratch = dir.path("scratch").join(run_id);
    fs::create_dir_all(&scratch).unwrap();
    fs::set_permissions(&scratch, fs::Permissions::from_mode(0o700)).unwrap();
    RunSpec {
        run_id: run_id.to_owned(),
        body: Body::Native(native),
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
        labels: BTreeMap::new(),
        bundle: None,
        network: Default::default(),
        ports: Vec::new(),
        host_ports: Vec::new(),
        log_sink: None,
        xcode: None,
    }
}

/// A native body: `program` and `args`, no environment, no network, no inputs or outputs.
pub fn native(program: &str, args: &[&str]) -> NativeSpec {
    NativeSpec {
        program: program.to_owned(),
        args: args.iter().map(|arg| (*arg).to_owned()).collect(),
        env: BTreeMap::new(),
        cwd: String::new(),
        inputs: Vec::new(),
        outputs: Vec::new(),
        network: NetworkLevel::Off,
        ports: Vec::new(),
    }
}

/// The arguments of `/bin/sh` for a job that waits until the file `gate` exists, then runs
/// the shell text `work`: a job that is surely still running when its start returns (the fake
/// supervisor is the job, and a later process knows it only while it runs or by its record).
pub fn gated(gate: &Path, work: &str) -> Vec<String> {
    let script = format!("while [ ! -e \"$1\" ]; do /bin/sleep 0.01; done; {work}");
    vec![
        "-c".to_owned(),
        script,
        "sh".to_owned(),
        gate.display().to_string(),
    ]
}

/// Opens `gate`: a job [`gated`] on it goes on.
pub fn open_gate(gate: &Path) {
    fs::write(gate, "").unwrap();
}

/// The value of the profile parameter `name` of a launch.
pub fn param<'a>(launched: &'a Launched, name: &str) -> Option<&'a str> {
    launched
        .params
        .iter()
        .find(|(given, _)| given == name)
        .map(|(_, value)| value.as_str())
}
