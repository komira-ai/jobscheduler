//! The macOS part of the inbound test: the helper this process serves, the runtime, the
//! anchor, the listener run and the three probes.

use std::fs;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::path::{Path, PathBuf};
use std::time::Duration;

use js_agent::lifecycle::{Body, RunHandle, RunSpec, Runtime, Status};
use js_agent::native::{Ceiling, NativeConfig, NativeRuntime};
use js_agent_wire::{Isolation, NativeSpec, NetworkLevel};
use js_mac_pool::seatbelt::{AbsPath, ReadModel};
use js_mac_provision::counters::{self, Which};
use js_mac_provision::pfctl::SystemPfctl;
use js_mac_provision::provision::Provisioner;
use js_mac_provision::{AnchorName, UidRange};
use js_mac_session::client::Client;

use crate::ROLE;
use crate::verdict::{self, CaseA, Probe, Ready, Rise, Verdict};

/// The variable naming the directory the test works in.
const ROOT: &str = "JS_NATIVE_INBOUND_ROOT";
/// The variable naming the one uid of the helper's range.
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

/// The helper this process serves, and where it keeps things.
struct Fixture {
    root: PathBuf,
    socket: PathBuf,
    homes: PathBuf,
    state: PathBuf,
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
        socket: root.join("sock").join("socket"),
        homes: root.join("homes"),
        state: root.join("state"),
        pf_state: root.join("pf-state"),
        root,
        uid,
    };
    fs::create_dir(&fixture.homes).unwrap();
    fs::create_dir(fixture.root.join("scratch")).unwrap();
    serve(&fixture);
    fixture
}

/// Serves the real helper on the fixture's socket, with a range of the one uid, for as
/// long as the process lives.
fn serve(fixture: &Fixture) {
    use std::os::unix::net::UnixStream;
    use std::sync::Arc;

    use clap::Parser as _;
    use js_mac_session::macos::MacHost;
    use js_mac_session::scan::ScanFolder;
    use js_mac_session::server::{self, Caller, CallerCheck};
    use js_mac_session::start::{self, Args};
    use js_mac_session::sweep::SweepPlan;

    /// The caller check of a helper this very process serves: everyone who reaches the
    /// socket is the test.
    struct Anyone;

    impl CallerCheck for Anyone {
        fn identify(&self, _: &UnixStream) -> Result<Caller, String> {
            Ok(Caller {
                token: Vec::new(),
                pid: 0,
                version: 0,
            })
        }

        fn check(&self, _: &Caller) -> Result<(), String> {
            Ok(())
        }
    }

    let path = |path: &Path| path.display().to_string();
    let args = Args::parse_from([
        "js-mac-session".to_owned(),
        "--socket".to_owned(),
        path(&fixture.socket),
        "--socket-group".to_owned(),
        "wheel".to_owned(),
        "--state-dir".to_owned(),
        path(&fixture.state),
        "--uid-range".to_owned(),
        format!("{0}-{0}", fixture.uid),
        "--homes".to_owned(),
        path(&fixture.homes),
        // The boot reset clears the auto-login of the Mac the helper serves; the files it
        // clears are the test's own, so a real login setting is not touched.
        "--kcpassword".to_owned(),
        path(&fixture.root.join("kcpassword")),
        "--loginwindow".to_owned(),
        path(&fixture.root.join("loginwindow")),
        "--daemon-requirement".to_owned(),
        "unused: the check is replaced".to_owned(),
    ]);
    let ready = start::prepare(
        &args,
        Box::new(MacHost),
        "js-native-inbound".to_owned(),
        SweepPlan::macos_schedules(),
        SweepPlan::macos(),
        ScanFolder::macos(),
    )
    .unwrap();
    let check: Arc<dyn CallerCheck> = Arc::new(Anyone);
    std::thread::spawn(move || server::serve(&ready.listener, &ready.helper, &check));
}

/// The production layout of the runtime with a network cap that lets a run listen, plus the
/// directory this binary is in as a tool root (the run executes it). Opened ungated: the test
/// installs and removes the anchor itself.
fn runtime(fixture: &Fixture, exe: &Path) -> NativeRuntime {
    let denied = |path: &Path| AbsPath::new(path).unwrap();
    let config = NativeConfig {
        homes: fs::canonicalize(&fixture.homes).unwrap(),
        network: Ceiling::On,
        tool_roots: vec![
            PathBuf::from("/usr/bin"),
            PathBuf::from("/bin"),
            exe.parent().unwrap().to_owned(),
        ],
        read_denies: vec![
            denied(&fixture.state),
            denied(&fixture.root.join("scratch")),
        ],
        read_model: ReadModel::Open,
        journal: fixture.root.join("native-leases"),
        max_file_bytes: 1 << 30,
        max_processes: 512,
        egress: None,
    };
    NativeRuntime::open_ungated(Client::new(&fixture.socket), config, 1).unwrap()
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
        labels: Default::default(),
        bundle: None,
        network: Default::default(),
        ports: Vec::new(),
        env: Vec::new(),
    }
}

/// The runtime and the listener run; dropping it ends the run, so a panic anywhere leaves no
/// user and no process behind.
struct Trial {
    rt: NativeRuntime,
    run: Option<RunHandle>,
}

impl Trial {
    /// Ends the run (the helper's `kill-uid`, then `user-delete`); nothing is done when it
    /// was ended already.
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
