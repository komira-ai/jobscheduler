//! The macOS part of the fence test: the fixture (the places the test makes), the account
//! pool, the runtime, the four processes and the printing of what they answered.

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;

use js_agent::lifecycle::{Body, RunHandle, RunSpec, Runtime, Status};
use js_agent::mac_accounts::{MacHost, Places, Pool, Settings, Slots};
use js_agent::mac_supervisor::SystemLauncher;
use js_agent::native::fence::PRIVACY_ONLY_PLACES;
use js_agent::native::{Accounts, Ceiling, NativeConfig, NativeRuntime};
use js_agent_wire::{Isolation, NativeSpec, NetworkLevel};
use js_mac_pool::seatbelt::{AbsPath, ReadModel};
use js_mac_session::disclaim::Program;
use js_mac_session::full_disk_access::FullDiskAccess;

use crate::verdict::{self, Outcomes, PRIVACY, Report, Verdict};
use crate::{LINK_TARGET, PLACES, ROLE, responsible};

/// The variable naming the directory the test works in.
const ROOT: &str = "JS_NATIVE_FENCE_ROOT";
/// The variable naming the one uid of the pool's slots.
const UID: &str = "JS_NATIVE_FENCE_UID";
/// The variable naming the uid of the user who runs the test (`cargo xtask mac-root-tests`
/// sets it): that user's files and folders are the places that are not root's.
const OWNER_UID: &str = "JS_NATIVE_FENCE_OWNER_UID";
/// The variable naming the home folder of that user; when it is not set the test asks the
/// system (`id -P`).
const OWNER_HOME: &str = "JS_NATIVE_FENCE_OWNER_HOME";
const NEEDS_ROOT: &str =
    "needs root: run the test binary as root (see the header of tests/native_fence/main.rs)";
/// The most polls the test makes for the job to end.
const POLLS: u32 = 50_000_000;
/// The libtest test every process executes, in the role the environment gives it.
const CHILD: &str = "child_of_the_runs";
/// The arguments of that execution.
const ARGS: [&str; 4] = ["--exact", CHILD, "--nocapture", "--test-threads=1"];
/// The job's name, and so its scratch directory.
const RUN: &str = "fence-job";
/// The folders of the invoking user's home that the privacy controls guard.
const OWNER_FOLDERS: [(&str, &str); 5] = [
    ("owner-documents", "Documents"),
    ("owner-desktop", "Desktop"),
    ("owner-mail", "Library/Mail"),
    ("owner-messages", "Library/Messages"),
    ("owner-safari", "Library/Safari"),
];

/// The account pool of the agent, the places the test makes and where it keeps them.
struct Fixture {
    root: PathBuf,
    accounts: PathBuf,
    homes: PathBuf,
    uid: u32,
    owner_uid: u32,
    owner_home: PathBuf,
}

/// The uid that `name` holds in the environment.
fn uid_var(name: &str) -> u32 {
    std::env::var(name)
        .unwrap_or_else(|_| panic!("{name} is not set"))
        .parse()
        .unwrap_or_else(|_| panic!("{name} is not a uid"))
}

/// The home folder of the user `uid`: [`OWNER_HOME`], or what `id -P` prints for it.
fn home_of(uid: u32) -> PathBuf {
    if let Some(home) = std::env::var_os(OWNER_HOME) {
        return PathBuf::from(home);
    }
    let output = Command::new("/usr/bin/id")
        .args(["-P", &uid.to_string()])
        .output()
        .unwrap_or_else(|why| panic!("id -P {uid}: {why}; set {OWNER_HOME}"));
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    // A passwd entry: name:password:uid:gid:class:change:expire:gecos:home:shell.
    match text.trim().split(':').nth(8) {
        Some(home) if home.starts_with('/') => PathBuf::from(home),
        _ => panic!("id -P {uid} printed {text:?}, no home folder; set {OWNER_HOME}"),
    }
}

/// Makes the directory `path` with mode 0755.
fn make_dir(path: &Path) {
    fs::create_dir_all(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// Writes the file `path` with `mode`.
fn make_file(path: &Path, mode: u32) {
    fs::write(path, "x").unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

/// Makes the fixture: the pool's directories as the inbound test makes them, a file only the
/// invoking user may read in a directory everyone may search, and a home of another user whose
/// modes are open (as `native_privileged` makes one).
fn fixture() -> Fixture {
    assert!(rustix::process::geteuid().is_root(), "{NEEDS_ROOT}");
    let root = std::env::var_os(ROOT).unwrap_or_else(|| panic!("{ROOT} is not set"));
    let root = fs::canonicalize(root).unwrap();
    let uid = uid_var(UID);
    assert!(
        (500..=60000).contains(&uid),
        "{UID} must be a uid from 500 to 60000, got {uid}"
    );
    let owner_uid = uid_var(OWNER_UID);
    let fixture = Fixture {
        accounts: root.join("accounts"),
        homes: root.join("homes"),
        owner_home: home_of(owner_uid),
        root,
        uid,
        owner_uid,
    };
    fs::create_dir(&fixture.homes).unwrap();
    make_dir(&fixture.accounts);
    fs::create_dir(fixture.root.join("scratch")).unwrap();
    for sub in ["folders", "at/tabs", "at/jobs", "Shared", "tmp", "launchd"] {
        fs::create_dir_all(fixture.root.join("places").join(sub)).unwrap();
    }
    let private = fixture.root.join("owner-private");
    make_dir(&private);
    make_file(&private.join("secret"), 0o600);
    std::os::unix::fs::chown(private.join("secret"), Some(fixture.owner_uid), None).unwrap();
    let other = fixture.homes.join("js-other-home");
    make_dir(&other);
    make_file(&other.join("f"), 0o644);
    make_dir(&fixture.root.join("links"));
    fixture
}

/// The pool's slots: the one uid of the fixture.
fn slots(fixture: &Fixture) -> Slots {
    Slots::new(fixture.uid, 1).unwrap_or_else(|why| panic!("{UID}: {why}"))
}

/// The pool's settings: the Mac's, with the one slot, no account kept ready, the homes under
/// the fixture and every place a sweep covers under the fixture too, so the test touches
/// nothing of the Mac's own.
fn settings(fixture: &Fixture) -> Settings {
    let places = fixture.root.join("places");
    let mut settings = Settings::macos(slots(fixture), 0);
    settings.homes = fixture.homes.clone();
    settings.user_folders = places.join("folders");
    settings.schedules = Places {
        named: vec![format!("{}/{{name}}", places.join("at/tabs").display())],
        owned: vec![places.join("at/jobs")],
    };
    settings.shared = Places {
        named: vec![format!(
            "{}/disabled.{{uid}}.plist",
            places.join("launchd").display()
        )],
        owned: vec![places.join("Shared"), places.join("tmp")],
    };
    settings
}

/// The real pool over the real operating system.
fn pool(fixture: &Fixture) -> Arc<Pool> {
    let pool = Pool::with_settings(
        &fixture.accounts,
        settings(fixture),
        &BTreeSet::new(),
        Arc::new(MacHost),
    );
    Arc::new(pool.unwrap_or_else(|why| panic!("the pool: {why}")))
}

/// The production layout of the runtime: every privacy-only place, the pool's state and the
/// scratch root read-denied, the homes of the fixture (the profile denies reads below them
/// and below root's home but for the job's own), plus the directory this binary is in as a
/// tool root (the job executes it). Opened ungated: no packet filter is involved.
fn runtime(fixture: &Fixture, exe: &Path) -> NativeRuntime {
    let denied = |path: &Path| AbsPath::new(path).unwrap();
    let mut read_denies: Vec<AbsPath> = PRIVACY_ONLY_PLACES
        .iter()
        .map(|place| denied(Path::new(place)))
        .collect();
    read_denies.push(denied(&fixture.accounts));
    read_denies.push(denied(&fixture.root.join("scratch")));
    let config = NativeConfig {
        accounts: Accounts {
            state: fixture.accounts.clone(),
            slots: slots(fixture),
            ready: 0,
        },
        homes: fs::canonicalize(&fixture.homes).unwrap(),
        network: Ceiling::Off,
        tool_roots: vec![
            PathBuf::from("/usr/bin"),
            PathBuf::from("/bin"),
            exe.parent().unwrap().to_owned(),
        ],
        read_denies,
        mount_roots: Vec::new(),
        read_model: ReadModel::Open,
        journal: fixture.root.join("native-leases"),
        max_file_bytes: 1 << 30,
        max_processes: 512,
        egress: None,
        full_disk_access: FullDiskAccess::Granted,
    };
    let launcher = Arc::new(SystemLauncher::at(supervisor_binary()));
    NativeRuntime::open_ungated(Some(pool(fixture)), launcher, config, 1).unwrap()
}

/// The places every process probes, as `(name, path)`, in the order of the names in
/// [`verdict`].
fn places(fixture: &Fixture) -> Vec<(&'static str, PathBuf)> {
    let mut places: Vec<(&'static str, PathBuf)> = PRIVACY
        .iter()
        .copied()
        .zip(PRIVACY_ONLY_PLACES.iter().copied().map(PathBuf::from))
        .collect();
    places.push(("root-home", PathBuf::from("/private/var/root")));
    places.push(("root-home-var", PathBuf::from("/var/root")));
    places.push((
        "other-user-private",
        fixture.root.join("owner-private").join("secret"),
    ));
    places.push(("other-home", fixture.homes.join("js-other-home")));
    for (name, folder) in OWNER_FOLDERS {
        places.push((name, fixture.owner_home.join(folder)));
    }
    places
}

/// The other paths to the privacy database every process tries (a symlink the test makes, the
/// lower-case spelling, the volume-and-inode path root stats), as `(name, path)`: root must
/// open them, the job must be refused them. The hard link and the clone are made by the job
/// itself, in its working directory.
fn evasions(fixture: &Fixture) -> Vec<(&'static str, PathBuf)> {
    let database = Path::new(PRIVACY_ONLY_PLACES[1]);
    let link = fixture.root.join("links").join("tcc-db");
    std::os::unix::fs::symlink(database, &link).unwrap();
    // A database that is not there gives a path that is not there either; root's own
    // answer then says why the run cannot be settled.
    let by_inode = fs::metadata(database).map_or_else(
        |_| PathBuf::from("/.vol/0/0"),
        |meta| PathBuf::from(format!("/.vol/{}/{}", meta.dev(), meta.ino())),
    );
    vec![
        ("evasion-symlink", link),
        (
            "evasion-lowercase",
            PathBuf::from(PRIVACY_ONLY_PLACES[1].to_lowercase()),
        ),
        ("evasion-vol", by_inode),
    ]
}

/// The text of the places variable: one `name=path` per line.
fn lines(places: &[(&str, PathBuf)]) -> String {
    places
        .iter()
        .map(|(name, path)| format!("{name}={}", path.display()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// How a probe process outside the runtime is started.
#[derive(Clone, Copy, Debug)]
enum Kind {
    /// The test's ordinary child, as root.
    Root,
    /// The slot's uid, started the ordinary way: it answers for the test.
    Counted,
    /// The slot's uid, started disclaimed: it answers for itself.
    Disclaimed,
}

/// Runs this binary in the role `probe` as `kind` asks, with no profile, and reads what it
/// printed. A process that could not be started or printed nothing gives an empty report,
/// which the verdict reads as missing lines.
fn probe(kind: Kind, exe: &Path, fixture: &Fixture, text: &str) -> Report {
    let gid = settings(fixture).gid;
    let output: std::io::Result<Output> = match kind {
        Kind::Root => Command::new(exe)
            .args(ARGS)
            .env(ROLE, "probe")
            .env(PLACES, text)
            .current_dir(&fixture.root)
            .output(),
        Kind::Counted => Command::new(exe)
            .args(ARGS)
            .env(ROLE, "probe")
            .env(PLACES, text)
            .current_dir(&fixture.root)
            .uid(fixture.uid)
            .gid(gid)
            .output(),
        Kind::Disclaimed => Program::new(exe, ARGS)
            .env(ROLE, "probe")
            .env(PLACES, text)
            .current_dir(&fixture.root)
            .uid(fixture.uid)
            .gid(gid)
            .output(),
    };
    match output {
        Ok(output) => {
            let report = verdict::parse_report(&String::from_utf8_lossy(&output.stdout));
            if report.who.is_none() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                eprintln!("{kind:?} probe: {}\n{stderr}", output.status);
            }
            report
        }
        Err(why) => {
            eprintln!("{kind:?} probe could not be started: {why}");
            Report::default()
        }
    }
}

/// The job: this binary in the role `probe`, with the evasions and the database to link.
fn spec(fixture: &Fixture, exe: &Path, text: &str) -> RunSpec {
    let scratch = fixture.root.join("scratch").join(RUN);
    fs::create_dir(&scratch).unwrap();
    RunSpec {
        run_id: RUN.to_owned(),
        body: Body::Native(NativeSpec {
            program: exe.display().to_string(),
            args: ARGS.map(str::to_owned).to_vec(),
            env: [
                (ROLE.to_owned(), "probe".to_owned()),
                (PLACES.to_owned(), text.to_owned()),
                (LINK_TARGET.to_owned(), PRIVACY_ONLY_PLACES[1].to_owned()),
            ]
            .into(),
            cwd: String::new(),
            inputs: Vec::new(),
            outputs: Vec::new(),
            network: NetworkLevel::Off,
            ports: Vec::new(),
        }),
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
        env: Vec::new(),
        log_sink: None,
        xcode: None,
    }
}

/// The runtime and the job's run; dropping it ends the run, so a panic anywhere leaves no
/// account and no process behind.
struct Trial {
    rt: NativeRuntime,
    run: Option<RunHandle>,
}

impl Trial {
    /// Ends the run (the job's group is stopped, the account's processes are killed and the
    /// account is deleted); nothing is done when it was ended already.
    fn end(&mut self) -> Result<(), String> {
        match self.run.take() {
            Some(id) => self
                .rt
                .remove(&id)
                .map_err(|why| format!("remove the job's run: {}", why.0)),
            None => Ok(()),
        }
    }

    /// Polls until the job's run is no longer running, with a bound on the polls, and
    /// returns its standard output and error.
    fn finish(&mut self) -> Result<(Status, String, String), String> {
        let id = self.run.clone().ok_or("the job's run was not started")?;
        for _ in 0..POLLS {
            let status = self
                .rt
                .poll(&id)
                .map_err(|why| format!("poll: {}", why.0))?;
            if status != Status::Running {
                let tail = self
                    .rt
                    .log_tail(&id)
                    .map_err(|why| format!("the job's log: {}", why.0))?
                    .ok_or("the job's run has no log")?;
                return Ok((status, tail.stdout, tail.stderr));
            }
            std::thread::yield_now();
        }
        Err("the job's run did not end".to_owned())
    }
}

impl Drop for Trial {
    fn drop(&mut self) {
        let _ = self.end();
    }
}

/// Prints what a process answered, one line per place.
fn show(label: &str, report: &Report) {
    println!("process {label}");
    for (name, answer) in &report.answers {
        println!("  {}", verdict::place_line(name, answer));
    }
    match &report.who {
        Some(who) => println!("  {}", verdict::who_line(who)),
        None => println!("  (no who line)"),
    }
}

/// Runs the test on a Mac: the three processes outside the runtime, then the job, prints what
/// each answered and the final line, ends the run, and returns the verdict.
pub fn run() -> Verdict {
    let fixture = fixture();
    let exe = fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
    let mut asked = places(&fixture);
    asked.extend(evasions(&fixture));
    let plain = lines(&asked);
    let test_responsible = responsible(std::process::id());

    let root = probe(Kind::Root, &exe, &fixture, &plain);
    let counted = probe(Kind::Counted, &exe, &fixture, &plain);
    let disclaimed = probe(Kind::Disclaimed, &exe, &fixture, &plain);

    let rt = runtime(&fixture, &exe);
    let mut trial = Trial { rt, run: None };
    let id = trial
        .rt
        .create_and_start(&spec(&fixture, &exe, &plain))
        .unwrap_or_else(|why| panic!("start the job's run: {}", why.0));
    trial.run = Some(id);
    let (status, stdout, stderr) = trial.finish().unwrap_or_else(|why| panic!("{why}"));
    let job = verdict::parse_report(&stdout);
    if job.who.is_none() {
        eprintln!("the job ended {status:?}\nstdout:\n{stdout}\nstderr:\n{stderr}");
    }
    trial.end().unwrap_or_else(|why| panic!("{why}"));

    let outcomes = Outcomes {
        test_responsible,
        slot_uid: fixture.uid,
        root,
        counted,
        disclaimed,
        job,
    };
    println!("the test's own responsible pid: {test_responsible:?}");
    show("root", &outcomes.root);
    show("slot uid, counted as the test", &outcomes.counted);
    show("slot uid, disclaimed", &outcomes.disclaimed);
    show("the job", &outcomes.job);
    let outcome = verdict::verdict(&outcomes);
    println!("{}", verdict::final_line(&outcome));
    outcome
}

/// The supervisor's program: the `js-agent` beside this test binary when there is one (an
/// operator copies both from the workflow's artifact), else the one cargo built for the test.
fn supervisor_binary() -> PathBuf {
    let beside = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("js-agent")))
        .filter(|path| path.is_file());
    beside.unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_js-agent")))
}
