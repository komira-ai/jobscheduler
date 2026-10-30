//! The native runtime for real, as root, on a Mac: a real `js-mac-session` helper (served
//! from this process, with its real macOS host) makes lease users, the runtime starts
//! programs as them under the Seatbelt profile, and the tests look at what the machine
//! shows: the uid a program runs as, the files it can reach, the processes a kill leaves,
//! the user a removal deletes. What a hosted runner cannot do (change users) is done here.
//!
//! Every test is `#[ignore]`: each needs root and creates users on the machine it runs
//! on. Build the binary on a Mac as an ordinary user (`cargo test -p js-agent --test
//! native_privileged --no-run`), or take it from the `js-agent-native-privileged-tests`
//! workflow, and run it as root with `JS_NATIVE_PRIV_ROOT` naming a new directory, owned
//! by root and mode 0755, that every user can search down to (a home directory is 0700 and
//! does not qualify), and `--ignored --test-threads=1 --nocapture`. The helper's users get
//! the uids 680 to 689, which must be free; the helper's state, its socket and the users'
//! homes are made below the directory. Never run it on a production host.
//!
//! The file builds on every platform, so clippy reads it on Linux; run anywhere but macOS,
//! each test stops at its first line.

use std::fs;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use js_agent::lifecycle::{Body, Exit, RunHandle, RunSpec, Runtime, Status};
use js_agent::native::{NativeConfig, NativeRuntime};
use js_agent_wire::{Isolation, NativeSpec, NetworkLevel};
use js_mac_pool::seatbelt::{AbsPath, Net, ReadModel};
use js_mac_session::client::Client;

/// The variable naming the directory the tests work in.
const ROOT: &str = "JS_NATIVE_PRIV_ROOT";
/// The uids the helper gives its users: free on the machine (the helper checks).
#[cfg(target_os = "macos")]
const UIDS: &str = "680-689";
const NEEDS_ROOT: &str =
    "needs root: run the test binary as root (see the header of tests/native_privileged.rs)";
/// The most polls a test makes for a run to end.
const POLLS: u32 = 50_000_000;
/// The most times a test lists the machine's processes while it waits for some to start
/// (each listing runs a program).
const WATCHES: u32 = 20_000;

/// The helper this process serves, and where it keeps things.
struct Fixture {
    root: PathBuf,
    socket: PathBuf,
    homes: PathBuf,
    state: PathBuf,
}

static FIXTURE: OnceLock<Fixture> = OnceLock::new();
/// Gives every runtime a term of its own: the helper never makes a user twice for a lease.
static TERMS: AtomicU64 = AtomicU64::new(1);

fn fixture() -> &'static Fixture {
    FIXTURE.get_or_init(|| {
        assert!(rustix::process::geteuid().is_root(), "{NEEDS_ROOT}");
        let root = std::env::var_os(ROOT).unwrap_or_else(|| panic!("{ROOT} is not set"));
        let root = fs::canonicalize(root).unwrap();
        let fixture = Fixture {
            socket: root.join("sock").join("socket"),
            homes: root.join("homes"),
            state: root.join("state"),
            root,
        };
        fs::create_dir(&fixture.homes).unwrap();
        fs::create_dir(fixture.root.join("scratch")).unwrap();
        serve(&fixture);
        fixture
    })
}

/// Serves the real helper on the fixture's socket, for as long as the process lives.
#[cfg(target_os = "macos")]
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
        UIDS.to_owned(),
        "--homes".to_owned(),
        path(&fixture.homes),
        "--daemon-requirement".to_owned(),
        "unused: the check is replaced".to_owned(),
    ]);
    let ready = start::prepare(
        &args,
        Box::new(MacHost),
        "js-native-privileged".to_owned(),
        SweepPlan::macos_schedules(),
        SweepPlan::macos(),
        ScanFolder::macos(),
    )
    .unwrap();
    let check: Arc<dyn CallerCheck> = Arc::new(Anyone);
    std::thread::spawn(move || server::serve(&ready.listener, &ready.helper, &check));
}

#[cfg(not(target_os = "macos"))]
fn serve(_: &Fixture) {
    panic!("these tests run on macOS only");
}

fn runtime(fixture: &Fixture) -> NativeRuntime {
    let config = NativeConfig {
        homes: fs::canonicalize(&fixture.homes).unwrap(),
        net: Net::Off,
        tool_roots: vec![PathBuf::from("/usr/bin"), PathBuf::from("/bin")],
        read_denies: vec![AbsPath::new(&fixture.state).unwrap()],
        read_model: ReadModel::Open,
    };
    let term = TERMS.fetch_add(1, Ordering::SeqCst);
    NativeRuntime::open(Client::new(&fixture.socket), config, term).unwrap()
}

/// A run of `program args` whose scratch directory (the log files) is made here.
fn spec(fixture: &Fixture, run_id: &str, program: &str, args: &[&str]) -> RunSpec {
    let scratch = fixture.root.join("scratch").join(run_id);
    fs::create_dir(&scratch).unwrap();
    RunSpec {
        run_id: run_id.to_owned(),
        body: Body::Native(NativeSpec {
            program: program.to_owned(),
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
            env: Default::default(),
            cwd: String::new(),
            inputs: Vec::new(),
            outputs: Vec::new(),
            network: NetworkLevel::Off,
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
    }
}

/// Polls until the run is no longer running.
fn settle(rt: &mut NativeRuntime, id: &RunHandle) -> Status {
    for _ in 0..POLLS {
        let status = rt.poll(id).unwrap();
        if status != Status::Running {
            return status;
        }
        std::thread::yield_now();
    }
    panic!("the run did not end");
}

fn exited(code: i32) -> Status {
    Status::Exited(Exit {
        code,
        oom_killed: false,
    })
}

/// The home of the user of `id`: the lease `T.S` is the user `kbf-lease-T-S`.
fn home_of(fixture: &Fixture, id: &RunHandle) -> PathBuf {
    let name = format!("kbf-lease-{}", id.0.replace('.', "-"));
    fs::canonicalize(&fixture.homes).unwrap().join(name)
}

/// The uids of every process on the machine.
fn process_uids() -> Vec<u32> {
    let out = Command::new("/bin/ps")
        .args(["-A", "-o", "uid="])
        .output()
        .unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    text.lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect()
}

/// Asserts that a program runs as a user of the helper's range and not as root, that it
/// writes in its run directory and the file is the user's, and that the same program is
/// refused a file in a directory every user may write: the profile is applied to a real
/// user. It catches a program that runs as root, one that runs unconfined, and a run
/// directory the user cannot use.
#[test]
#[ignore = "needs root and creates users"]
fn a_program_runs_as_a_lease_user_under_the_profile() {
    let fixture = fixture();
    let mut rt = runtime(fixture);
    let id = rt
        .create_and_start(&spec(fixture, "uid", "/usr/bin/id", &["-u"]))
        .unwrap();
    assert_eq!(settle(&mut rt, &id), exited(0));
    let printed = rt.log_tail(&id).unwrap().unwrap().stdout;
    let uid: u32 = printed.trim().parse().unwrap();
    assert!((680..=689).contains(&uid), "ran as uid {uid}");
    assert_eq!(fs::metadata(home_of(fixture, &id)).unwrap().uid(), uid);
    rt.remove(&id).unwrap();

    let id = rt
        .create_and_start(&spec(
            fixture,
            "write",
            "/bin/sh",
            &["-c", "echo data > made"],
        ))
        .unwrap();
    assert_eq!(settle(&mut rt, &id), exited(0));
    let made = fs::metadata(home_of(fixture, &id).join("exec").join("made")).unwrap();
    assert!((680..=689).contains(&made.uid()));
    rt.remove(&id).unwrap();

    let outside = format!("/private/tmp/js-native-priv-{}", std::process::id());
    let script = format!("echo data > {outside}");
    let id = rt
        .create_and_start(&spec(fixture, "outside", "/bin/sh", &["-c", &script]))
        .unwrap();
    let status = settle(&mut rt, &id);
    let written = Path::new(&outside).exists();
    let _ = fs::remove_file(&outside);
    assert!(!written, "the profile let the program write {outside}");
    assert_eq!(status, exited(1));
    rt.remove(&id).unwrap();
}

/// Asserts that a stop ends every process of the user, not only the one started (a shell
/// that backgrounds two sleeps leaves three), that the removal deletes the user and its
/// home, and that the machine has no such user afterwards. It catches a kill that leaves
/// children running as a lease uid and a user left behind.
#[test]
#[ignore = "needs root and creates users"]
fn a_stop_ends_every_process_of_the_user_and_a_removal_deletes_it() {
    let fixture = fixture();
    let mut rt = runtime(fixture);
    let script = "/bin/sleep 600 & /bin/sleep 600 & wait";
    let id = rt
        .create_and_start(&spec(fixture, "kill", "/bin/sh", &["-c", script]))
        .unwrap();
    let home = home_of(fixture, &id);
    let uid = fs::metadata(&home).unwrap().uid();
    assert_eq!(rt.poll(&id).unwrap(), Status::Running);
    // The shell starts its two sleeps a moment after the run is reported started.
    let mut started = false;
    for _ in 0..WATCHES {
        if process_uids().iter().filter(|one| **one == uid).count() >= 3 {
            started = true;
            break;
        }
        std::thread::yield_now();
    }
    assert!(
        started,
        "the shell and its two sleeps never ran as uid {uid}"
    );
    rt.stop(&id, Duration::from_secs(10)).unwrap();
    assert_eq!(settle(&mut rt, &id), exited(137));
    assert!(
        !process_uids().contains(&uid),
        "a process of the user is left"
    );
    rt.remove(&id).unwrap();
    assert!(!home.exists());
    let user = home.file_name().unwrap().to_str().unwrap().to_owned();
    let known = Command::new("/usr/bin/id")
        .args(["-u", &user])
        .output()
        .unwrap();
    assert!(!known.status.success(), "the user {user} still exists");
}

/// Asserts that one run cannot read another run's files: the first writes a file in its
/// run directory, the second (another user) is refused it, and root reads it, which shows
/// the path is right and the refusal is the uid's. It catches homes that other lease users
/// can enter.
#[test]
#[ignore = "needs root and creates users"]
fn one_runs_files_are_closed_to_another_run() {
    let fixture = fixture();
    let mut rt = runtime(fixture);
    let first = rt
        .create_and_start(&spec(fixture, "a", "/bin/sh", &["-c", "echo secret > s"]))
        .unwrap();
    assert_eq!(settle(&mut rt, &first), exited(0));
    let file = home_of(fixture, &first).join("exec").join("s");
    assert_eq!(fs::read_to_string(&file).unwrap(), "secret\n");
    let path = file.display().to_string();
    let second = rt
        .create_and_start(&spec(fixture, "b", "/bin/cat", &[&path]))
        .unwrap();
    assert_eq!(settle(&mut rt, &second), exited(1));
    let tail = rt.log_tail(&second).unwrap().unwrap();
    assert_eq!(tail.stdout, "", "the second run read the first's file");
    assert!(tail.stderr.contains("Permission denied"), "{}", tail.stderr);
    rt.remove(&first).unwrap();
    rt.remove(&second).unwrap();
}
