//! The macOS part of the inbound test: the account pool, the runtime, the anchor, the
//! listener run and the three probes.

use std::collections::BTreeSet;
use std::fs;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use js_agent::lifecycle::{Body, RunHandle, RunSpec, Runtime, Status};
use js_agent::mac_accounts::{MacHost, Places, Pool, Settings, Slots};
use js_agent::mac_supervisor::SystemLauncher;
use js_agent::native::{Accounts, Ceiling, NativeConfig, NativeRuntime};
use js_agent_wire::{Isolation, NativeSpec, NetworkLevel};
use js_mac_pool::seatbelt::{AbsPath, ReadModel};
use js_mac_provision::counters::{self, Which};
use js_mac_provision::pfctl::SystemPfctl;
use js_mac_provision::provision::Provisioner;
use js_mac_provision::{AnchorName, UidRange};
use js_mac_session::full_disk_access::FullDiskAccess;

use crate::ROLE;
use crate::verdict::{self, CaseA, Probe, Ready, Rise, Verdict};

/// The variable naming the directory the test works in.
const ROOT: &str = "JS_NATIVE_INBOUND_ROOT";
/// The variable naming the one uid of the pool's slots.
const UID: &str = "JS_NATIVE_INBOUND_UID";
const NEEDS_ROOT: &str =
    "needs root: run the test binary as root (see the header of tests/native_inbound_pf/main.rs)";
/// The most polls the test makes for the listener to report its port.
const POLLS: u32 = 5_000_000;
/// The libtest test the run executes.
const CHILD: &str = "child_of_the_runs";
/// The test's own anchor.
const ANCHOR: &str = "com.apple/js-agent-inbound-test";
/// The bound of one connect: it only ends a probe nobody answers, nothing is measured by it.
const CONNECT_BOUND: Duration = Duration::from_millis(2000);
/// The run's name, and so its scratch directory.
const RUN: &str = "listener";

/// The account pool of the agent and where it keeps things.
struct Fixture {
    root: PathBuf,
    accounts: PathBuf,
    homes: PathBuf,
    pf_state: PathBuf,
    uid: u32,
}

fn fixture() -> Fixture {
    assert!(rustix::process::geteuid().is_root(), "{NEEDS_ROOT}");
    let root = std::env::var_os(ROOT).unwrap_or_else(|| panic!("{ROOT} is not set"));
    let root = fs::canonicalize(root).unwrap();
    let uid: u32 = std::env::var(UID)
        .unwrap_or_else(|_| panic!("{UID} is not set"))
        .parse()
        .unwrap_or_else(|_| panic!("{UID} is not a uid"));
    assert!(
        (500..=60000).contains(&uid),
        "{UID} must be a uid from 500 to 60000, got {uid}"
    );
    let fixture = Fixture {
        accounts: root.join("accounts"),
        homes: root.join("homes"),
        pf_state: root.join("pf-state"),
        root,
        uid,
    };
    fs::create_dir(&fixture.homes).unwrap();
    fs::create_dir(&fixture.accounts).unwrap();
    // The pool wants its state directory owned by root and writable by no one else.
    fs::set_permissions(&fixture.accounts, fs::Permissions::from_mode(0o755)).unwrap();
    fs::create_dir(fixture.root.join("scratch")).unwrap();
    for sub in ["folders", "at/tabs", "at/jobs", "Shared", "tmp", "launchd"] {
        fs::create_dir_all(fixture.root.join("places").join(sub)).unwrap();
    }
    fixture
}

/// The pool's slots: the one uid of the fixture.
fn slots(fixture: &Fixture) -> Slots {
    Slots::new(fixture.uid, 1).unwrap_or_else(|why| panic!("{UID}: {why}"))
}

/// The pool's settings: the Mac's, with the one slot, no account kept ready (so a removal
/// leaves no user behind), the homes under the fixture and every place a sweep covers under
/// the fixture too, so the test touches nothing of the Mac's own.
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

/// The production layout of the runtime with a network cap that lets a run listen, plus the
/// directory this binary is in as a tool root (the run executes it). Opened ungated: the test
/// installs and removes the anchor itself.
fn runtime(fixture: &Fixture, exe: &Path) -> NativeRuntime {
    let denied = |path: &Path| AbsPath::new(path).unwrap();
    let config = NativeConfig {
        accounts: Accounts {
            state: fixture.accounts.clone(),
            slots: slots(fixture),
            ready: 0,
        },
        homes: fs::canonicalize(&fixture.homes).unwrap(),
        network: Ceiling::On,
        host_ports: Default::default(),
        tool_roots: vec![
            PathBuf::from("/usr/bin"),
            PathBuf::from("/bin"),
            exe.parent().unwrap().to_owned(),
        ],
        read_denies: vec![
            denied(&fixture.accounts),
            denied(&fixture.root.join("scratch")),
        ],
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

/// The run of this binary's `child_of_the_runs` in the role `listen`; its scratch directory
/// (the log files) is made here.
fn spec(fixture: &Fixture, exe: &Path) -> RunSpec {
    let scratch = fixture.root.join("scratch").join(RUN);
    fs::create_dir(&scratch).unwrap();
    RunSpec {
        run_id: RUN.to_owned(),
        body: Body::Native(NativeSpec {
            program: exe.display().to_string(),
            args: ["--exact", CHILD, "--nocapture", "--test-threads=1"]
                .map(str::to_owned)
                .to_vec(),
            env: [(ROLE.to_owned(), "listen".to_owned())].into(),
            cwd: String::new(),
            inputs: Vec::new(),
            outputs: Vec::new(),
            network: NetworkLevel::On,
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

/// The runtime and the listener run; dropping it ends the run, so a panic anywhere leaves no
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
                .map_err(|why| format!("remove the listener run: {}", why.0)),
            None => Ok(()),
        }
    }
}

impl Drop for Trial {
    fn drop(&mut self) {
        let _ = self.end();
    }
}

/// Removes the anchor whatever happens to the test.
struct Loaded {
    anchor: AnchorName,
    state: PathBuf,
}

impl Drop for Loaded {
    fn drop(&mut self) {
        let pf = SystemPfctl::new("/sbin/pfctl");
        if let Err(e) = Provisioner::new(&pf, &self.anchor, &self.state).remove() {
            eprintln!("could not remove the anchor {}: {e}", self.anchor);
        }
    }
}

/// Polls the listener's stdout log until it reports its uid and port, the run ends, or the
/// count of polls runs out.
fn wait_ready(trial: &mut Trial, fixture: &Fixture) -> Result<Ready, String> {
    let id = trial
        .run
        .clone()
        .ok_or("the listener run was not started")?;
    let dir = fixture.root.join("scratch").join(RUN);
    for _ in 0..POLLS {
        let stdout = fs::read_to_string(dir.join("stdout")).unwrap_or_default();
        if let Some(ready) = verdict::parse_ready(&stdout)? {
            return Ok(ready);
        }
        let status = trial
            .rt
            .poll(&id)
            .map_err(|why| format!("poll: {}", why.0))?;
        if status != Status::Running {
            let stderr = fs::read_to_string(dir.join("stderr")).unwrap_or_default();
            return Err(format!(
                "the listener run ended {status:?} before it reported its port\nstdout:\n{stdout}\nstderr:\n{stderr}"
            ));
        }
        std::thread::yield_now();
    }
    Err("the listener run did not report its port".to_owned())
}

/// This Mac's primary IPv4 address: the source address the kernel picks for a route to a
/// documentation address. A UDP connect sends nothing.
fn own_address() -> Ipv4Addr {
    let socket = UdpSocket::bind("0.0.0.0:0").expect("bind a UDP socket");
    socket
        .connect("192.0.2.1:9")
        .expect("this Mac has no route to send from (no default route?)");
    match socket.local_addr().expect("local address") {
        SocketAddr::V4(a) => *a.ip(),
        SocketAddr::V6(a) => panic!("expected an IPv4 address, got {a}"),
    }
}

/// One connect from this process to `addr`.
fn connect(addr: SocketAddr) -> Probe {
    let result = TcpStream::connect_timeout(&addr, CONNECT_BOUND).map(|_| ());
    Probe::from_connect(&result)
}

/// Runs the test on a Mac: prints a line per case and the final line, ends the run and
/// removes the anchor, and returns the verdict.
pub fn run() -> Verdict {
    let fixture = fixture();
    let exe = fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
    let rt = runtime(&fixture, &exe);
    let mut trial = Trial { rt, run: None };
    let id = trial
        .rt
        .create_and_start(&spec(&fixture, &exe))
        .unwrap_or_else(|why| panic!("start the listener run: {}", why.0));
    trial.run = Some(id);
    let ready = wait_ready(&mut trial, &fixture).unwrap_or_else(|why| panic!("{why}"));
    assert_eq!(
        ready.uid, fixture.uid,
        "the listener ran as uid {}, not the uid {} asked for",
        ready.uid, fixture.uid
    );

    let own = own_address();
    let primary = SocketAddr::new(IpAddr::V4(own), ready.port);
    let loopback = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), ready.port);

    let anchor = AnchorName::new(ANCHOR).expect("anchor name");
    let uids = UidRange::new(fixture.uid, 1).expect("uid range");
    let pf = SystemPfctl::new("/sbin/pfctl");
    let provisioner = Provisioner::new(&pf, &anchor, &fixture.pf_state);
    let _loaded = Loaded {
        anchor: anchor.clone(),
        state: fixture.pf_state.clone(),
    };
    provisioner.install(uids, &[]).expect("install the anchor");
    provisioner
        .check(uids, &[])
        .expect("the loaded rules are the rendered ones");

    // Case a: the primary address with the anchor in force; the counter decides a block.
    let before = counters::read(&pf, &anchor);
    let probe = connect(primary);
    let after = counters::read(&pf, &anchor);
    let rise = match (before, after) {
        (Ok(b), Ok(a)) => Rise::Rose(counters::rise(&b, &a, fixture.uid, Which::CatchAll)),
        (Err(e), _) | (_, Err(e)) => Rise::Unread(e.to_string()),
    };
    let case_a = CaseA { probe, rise };
    println!("{}", verdict::case_a_line(&case_a));

    // Case b: the listener is up, seen on loopback with the anchor in force.
    let case_b = connect(loopback);
    println!("{}", verdict::control_line('b', &case_b));

    // Case c: without the anchor the primary address connects.
    provisioner.remove().expect("remove the anchor");
    let case_c = connect(primary);
    println!("{}", verdict::control_line('c', &case_c));

    let outcome = verdict::verdict(&case_a, &case_b, &case_c);
    println!("{}", verdict::final_line(&outcome));
    trial.end().unwrap_or_else(|why| panic!("{why}"));
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
