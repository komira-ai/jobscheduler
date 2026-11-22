//! The native runtime for real, as root, on a Mac: the agent's own account pool (over the
//! real macOS host, `kbf-acct-<n>` accounts) lends each run a fresh account, the real
//! supervisor (this package's `js-agent`, started as `__supervise-native`) drops to it and
//! applies the Seatbelt profile, and the tests look at what the machine shows: the uid a
//! program runs as, the files it can reach, the processes a stop and a removal leave, the
//! account a removal deletes. What a hosted runner cannot do (change users) is done here.
//!
//! Every test is `#[ignore]`: each needs root and creates accounts on the machine it runs
//! on. Build the binary on a Mac as an ordinary user (`cargo test -p js-agent --test
//! native_privileged --no-run`) and run it as root, from the machine that built it (the
//! supervisor is the `js-agent` binary cargo built beside it, named at build time), with
//! `JS_NATIVE_PRIV_ROOT` naming a new directory, owned by root and mode 0755, that every
//! user can search down to (a home directory is 0700 and does not qualify), and `--ignored
//! --test-threads=1 --nocapture`. The pool's slots are the uids 4300 to 4309, which must be
//! free (outside the farm's 600 to 799), and no account of the pool may exist: the names are
//! `kbf-acct-<n>`, made fresh for each run and deleted at its removal (`cargo xtask
//! mac-root-tests` deletes what is left). The pool's state (its ledger) and the accounts'
//! homes are made below the directory; the accounts' per-user folders are the system's, in
//! `/private/var/folders`. Never run it on a production host.
//!
//! The file builds on every platform, so clippy reads it on Linux; run anywhere but macOS,
//! each test stops at its first line.

#![cfg(target_os = "macos")]

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use clap::Parser as _;
use js_agent::bundle::{BundleHost, Fetch, HostXcode, Needs, Placer, Settings, SystemHost};
use js_agent::lifecycle::{Body, Exit, RunHandle, RunSpec, Runtime, Status};
use js_agent::mac_accounts::{MacHost, Pool, Slots};
use js_agent::mac_supervisor::SystemLauncher;
use js_agent::native::{Accounts, Ceiling, NativeConfig, NativeRuntime};
use js_agent::{Cli, Platform};
use js_agent_wire::{Isolation, NativeSpec, NetworkLevel};
use js_bundle::pack_layer;
use js_mac_pool::seatbelt::{AbsPath, ReadModel};
use js_mac_session::full_disk_access::FullDiskAccess;
use js_oci::{Allow, Digest};
use js_testkit::oci_registry::{Artifact, Fixture as Registry};
use js_treestore::Sealer;
use serde_json::json;

/// The variable naming the directory the tests work in.
const ROOT: &str = "JS_NATIVE_PRIV_ROOT";

/// The VM runtime for real; macOS only, as it boots a VM.
#[cfg(target_os = "macos")]
#[path = "native_privileged/vm.rs"]
mod vm;
/// The uids of the pool's slots: free on the machine, and outside the farm's range (600
/// to 799).
const POOL_UIDS: std::ops::RangeInclusive<u32> = 4300..=4309;
/// The prefix of the accounts' names.
const ACCOUNT_PREFIX: &str = "kbf-acct-";
const NEEDS_ROOT: &str =
    "needs root: run the test binary as root (see the header of tests/native_privileged.rs)";
/// The most polls a test makes for a run to end.
const POLLS: u32 = 50_000_000;
/// The most times a test lists the machine's processes while it waits for some to start
/// (each listing runs a program).
const WATCHES: u32 = 20_000;

/// Where the fixture keeps things: the pool's state and the accounts' homes.
struct Fixture {
    root: PathBuf,
    homes: PathBuf,
    state: PathBuf,
}

static FIXTURE: OnceLock<Fixture> = OnceLock::new();
/// Gives every runtime a term of its own: a lease is never used twice.
static TERMS: AtomicU64 = AtomicU64::new(1);

fn fixture() -> &'static Fixture {
    FIXTURE.get_or_init(|| {
        assert!(rustix::process::geteuid().is_root(), "{NEEDS_ROOT}");
        let root = std::env::var_os(ROOT).unwrap_or_else(|| panic!("{ROOT} is not set"));
        let root = fs::canonicalize(root).unwrap();
        let fixture = Fixture {
            homes: root.join("homes"),
            state: root.join("state"),
            root,
        };
        for dir in [&fixture.homes, &fixture.state] {
            fs::create_dir(dir).unwrap();
            fs::set_permissions(dir, fs::Permissions::from_mode(0o755)).unwrap();
        }
        fs::create_dir(fixture.root.join("scratch")).unwrap();
        for name in ["secret", "open"] {
            let folder = fixture.root.join(name);
            fs::create_dir(&folder).unwrap();
            fs::write(folder.join("f"), "x").unwrap();
        }
        fixture
    })
}

fn runtime(fixture: &Fixture) -> NativeRuntime {
    runtime_with(fixture, |_| {})
}

/// The pool of the fixture over the real host: the slots of [`POOL_UIDS`], homes below the
/// fixture's directory, its state there too, and none ready ahead of a run, so the one
/// account a live run holds is the only one there is.
fn pool(fixture: &Fixture) -> Arc<Pool> {
    let mut settings = js_agent::mac_accounts::Settings::macos(slots(), 0);
    settings.homes = fs::canonicalize(&fixture.homes).unwrap();
    let pool = Pool::with_settings(
        &fixture.state,
        settings,
        &BTreeSet::new(),
        Arc::new(MacHost),
    );
    Arc::new(pool.unwrap_or_else(|why| panic!("the account pool: {why}")))
}

/// The slots of the pool: the uids 4300 to 4309.
fn slots() -> Slots {
    Slots::new(*POOL_UIDS.start(), 10).unwrap()
}

/// The production layout of the read denies: the state root and the scratch root (the runs'
/// standard-stream files are in it), and a directory of the test's own that file modes
/// leave open to everyone, so only the profile closes it.
fn runtime_with(fixture: &Fixture, adjust: impl FnOnce(&mut NativeConfig)) -> NativeRuntime {
    let denied = |path: &Path| AbsPath::new(path).unwrap();
    let mut config = NativeConfig {
        accounts: Accounts {
            state: fixture.state.clone(),
            slots: slots(),
            ready: 0,
        },
        homes: fs::canonicalize(&fixture.homes).unwrap(),
        network: Ceiling::Off,
        tool_roots: vec![PathBuf::from("/usr/bin"), PathBuf::from("/bin")],
        read_denies: vec![
            denied(&fixture.state),
            denied(&fixture.root.join("scratch")),
            denied(&fixture.root.join("secret")),
        ],
        mount_roots: Vec::new(),
        read_model: ReadModel::Open,
        journal: fixture.root.join("native-leases"),
        max_file_bytes: 1 << 30,
        max_processes: 512,
        egress: None,
        full_disk_access: FullDiskAccess::Granted,
    };
    adjust(&mut config);
    let term = TERMS.fetch_add(1, Ordering::SeqCst);
    let launcher = SystemLauncher::at(supervisor_binary());
    NativeRuntime::over(Some(pool(fixture)), Arc::new(launcher), config, term).unwrap()
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

/// The home of the account of the live run: the pool makes one account for a run and none
/// ahead of it, so the one `kbf-acct-<n>` directory below the homes is the run's.
fn home_of(fixture: &Fixture) -> PathBuf {
    let homes = fs::canonicalize(&fixture.homes).unwrap();
    let mut accounts: Vec<PathBuf> = fs::read_dir(&homes)
        .unwrap()
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(ACCOUNT_PREFIX)
        })
        .map(|entry| entry.path())
        .collect();
    assert_eq!(accounts.len(), 1, "accounts below the homes: {accounts:?}");
    accounts.remove(0)
}

/// The name of the account whose home is `home`.
fn account_of(home: &Path) -> String {
    home.file_name().unwrap().to_str().unwrap().to_owned()
}

/// Whether the machine has a user record named `name`.
fn has_record(name: &str) -> bool {
    Command::new("/usr/bin/dscl")
        .args([".", "-read", &format!("/Users/{name}")])
        .output()
        .unwrap()
        .status
        .success()
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

/// Asserts that a program runs as an account of the pool's slots and not as root (its
/// `USER` is the account's name), that it writes in its run directory and the file is the
/// account's, and that the same program is refused a file in a directory every user may
/// write: the profile is applied to a real account. It catches a program that runs as root,
/// one that runs unconfined, and a run directory the account cannot use.
#[test]
#[ignore = "needs root and creates users"]
fn a_program_runs_as_a_slot_account_under_the_profile() {
    let fixture = fixture();
    let mut rt = runtime(fixture);
    let id = rt
        .create_and_start(&spec(fixture, "uid", "/usr/bin/id", &["-u"]))
        .unwrap();
    assert_eq!(settle(&mut rt, &id), exited(0));
    let printed = rt.log_tail(&id).unwrap().unwrap().stdout;
    let uid: u32 = printed.trim().parse().unwrap();
    assert!(POOL_UIDS.contains(&uid), "ran as uid {uid}");
    assert_eq!(fs::metadata(home_of(fixture)).unwrap().uid(), uid);
    rt.remove(&id).unwrap();

    let id = rt
        .create_and_start(&spec(
            fixture,
            "write",
            "/bin/sh",
            &["-c", "echo data > made; echo $USER"],
        ))
        .unwrap();
    assert_eq!(settle(&mut rt, &id), exited(0));
    let home = home_of(fixture);
    let made = fs::metadata(home.join("exec").join("made")).unwrap();
    assert!(POOL_UIDS.contains(&made.uid()));
    let user = rt.log_tail(&id).unwrap().unwrap().stdout;
    assert_eq!(user.trim(), account_of(&home));
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

/// Waits until `count` processes run as `uid` (each listing runs a program), or fails.
fn wait_for_processes(uid: u32, count: usize) {
    for _ in 0..WATCHES {
        if process_uids().iter().filter(|one| **one == uid).count() >= count {
            return;
        }
        std::thread::yield_now();
    }
    panic!("{count} processes never ran as uid {uid}");
}

/// Asserts that a stop that the job ignores ends every process of the account, not only the
/// one started, and that the removal deletes the account: a shell that ignores `SIGTERM`
/// and backgrounds two sleeps (the supervisor, the shell and two sleeps run as the uid) is
/// stopped with no grace, so the next poll ends the group with `SIGKILL` (exit 137); no
/// process of the uid is left; the removal deletes the record and the home, and the uid
/// belongs to no one. It catches a kill that leaves children running as a slot uid, an
/// account that outlives its run, and a home that keeps what the run left.
#[test]
#[ignore = "needs root and creates users"]
fn a_stop_ends_every_process_of_the_account_and_a_removal_deletes_it() {
    let fixture = fixture();
    let mut rt = runtime(fixture);
    let script = "trap '' TERM; /bin/sleep 600 & /bin/sleep 600 & wait";
    let id = rt
        .create_and_start(&spec(fixture, "kill", "/bin/sh", &["-c", script]))
        .unwrap();
    let home = home_of(fixture);
    let name = account_of(&home);
    let uid = fs::metadata(&home).unwrap().uid();
    assert!(has_record(&name), "the account {name} has no record");
    assert_eq!(rt.poll(&id).unwrap(), Status::Running);
    // The shell starts its two sleeps a moment after the run is reported started.
    wait_for_processes(uid, 4);
    rt.stop(&id, Duration::ZERO).unwrap();
    assert_eq!(settle(&mut rt, &id), exited(137));
    rt.remove(&id).unwrap();
    assert!(
        !process_uids().contains(&uid),
        "a process of the account is left"
    );
    assert!(!home.exists(), "the home of {name} is left");
    assert!(!has_record(&name), "the record of {name} is left");
}

/// Asserts that a stop first sends `SIGTERM` to the job's process group, so the job may end
/// on its own: a shell with a sleep in the background ends of it at once (exit 143) though
/// the grace is long, and the sleep with it. It catches a stop that only ever kills.
#[test]
#[ignore = "needs root and creates users"]
fn a_stop_sends_term_to_the_whole_job_group() {
    let fixture = fixture();
    let mut rt = runtime(fixture);
    let id = rt
        .create_and_start(&spec(
            fixture,
            "term",
            "/bin/sh",
            &["-c", "/bin/sleep 600 & wait"],
        ))
        .unwrap();
    let uid = fs::metadata(home_of(fixture)).unwrap().uid();
    wait_for_processes(uid, 3);
    rt.stop(&id, Duration::from_secs(600)).unwrap();
    assert_eq!(settle(&mut rt, &id), exited(143));
    rt.remove(&id).unwrap();
    assert!(
        !process_uids().contains(&uid),
        "a process of the account is left"
    );
}

/// Asserts that one run cannot read another run's files: the first writes a file in its
/// run directory and is not removed, the second (another account) is refused it, and root
/// reads it, which shows the path is right and the refusal is the uid's. It catches homes that other slot accounts can enter.
#[test]
#[ignore = "needs root and creates users"]
fn one_runs_files_are_closed_to_another_run() {
    let fixture = fixture();
    let mut rt = runtime(fixture);
    let first = rt
        .create_and_start(&spec(fixture, "a", "/bin/sh", &["-c", "echo secret > s"]))
        .unwrap();
    assert_eq!(settle(&mut rt, &first), exited(0));
    let first_home = home_of(fixture);
    let file = first_home.join("exec").join("s");
    assert_eq!(fs::read_to_string(&file).unwrap(), "secret\n");
    let path = file.display().to_string();
    // The first account is kept (the run is not removed), so the second run is lent another.
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

/// Asserts a read deny that file modes do not explain, as a real slot account: the file in the
/// denied directory is world-readable (root made it, mode 0644, in a directory every user
/// may search), a file beside it outside the denied directory is read by the same account, and
/// the profile refuses the first. It also shows the production layout working: the scratch
/// root is denied and what the runs print still reaches their logs. It catches a read deny
/// that is only the mode of a directory.
#[test]
#[ignore = "needs root and creates users"]
fn a_read_deny_is_refused_to_a_real_account_the_modes_would_let_in() {
    let fixture = fixture();
    let mut rt = runtime(fixture);
    let open = fixture.root.join("open").join("f").display().to_string();
    let secret = fixture.root.join("secret").join("f").display().to_string();
    let id = rt
        .create_and_start(&spec(fixture, "readok", "/bin/cat", &[&open]))
        .unwrap();
    assert_eq!(
        settle(&mut rt, &id),
        exited(0),
        "the control file is readable"
    );
    assert_eq!(rt.log_tail(&id).unwrap().unwrap().stdout, "x");
    rt.remove(&id).unwrap();
    let id = rt
        .create_and_start(&spec(fixture, "readno", "/bin/cat", &[&secret]))
        .unwrap();
    assert_eq!(settle(&mut rt, &id), exited(1));
    let tail = rt.log_tail(&id).unwrap().unwrap();
    assert_eq!(tail.stdout, "");
    assert!(
        tail.stderr.contains("Operation not permitted"),
        "{}",
        tail.stderr
    );
    rt.remove(&id).unwrap();
}

/// Whether anything below `dir` (links not followed) belongs to `uid`.
fn owned_by(uid: u32, dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let meta = fs::symlink_metadata(entry.path()).unwrap();
        meta.uid() == uid || (meta.is_dir() && owned_by(uid, &entry.path()))
    })
}

/// Makes a System V shared memory segment and leaves it.
const SYSV_SHM: &str =
    r#"my $id = shmget(0, 64, 0600 | 01000); print defined $id ? "created\n" : "denied\n""#;

/// Asserts what a reused uid finds. The pool gives the slot after the one it gave last,
/// wrapping, so with ten slots the eleventh run gets the first run's uid, as a new account.
/// The first run makes a System V shared memory segment and leaves it: the profile does not
/// deny System V IPC, so the segment is made, and the test requires that once the account
/// is retired no IPC object belongs to its uid, its home and record are gone, and no file
/// below the fixture's directory belongs to it. It fails, and says so, when the removal
/// leaves the segment: that is the open risk, measured here. It catches a run that finds
/// what an earlier run on its uid left.
#[test]
#[ignore = "needs root and creates users"]
fn a_reused_uid_sees_nothing_of_the_previous_run() {
    let fixture = fixture();
    let mut rt = runtime(fixture);
    let mut uids = Vec::new();
    for n in 0..11 {
        let (program, args): (&str, Vec<&str>) = match n {
            0 => ("/usr/bin/perl", vec!["-e", SYSV_SHM]),
            _ => ("/usr/bin/id", vec!["-u"]),
        };
        let id = rt
            .create_and_start(&spec(fixture, &format!("reuse{n}"), program, &args))
            .unwrap();
        assert_eq!(settle(&mut rt, &id), exited(0));
        let printed = rt.log_tail(&id).unwrap().unwrap().stdout;
        if n == 0 {
            println!("PROBE the first run's shmget printed {printed:?}");
        }
        let home = home_of(fixture);
        let name = account_of(&home);
        uids.push(fs::metadata(&home).unwrap().uid());
        rt.remove(&id).unwrap();
        assert!(!home.exists(), "the home of {name} is left after run {n}");
        assert!(!has_record(&name), "the record of {name} is left");
        let ipcs = Command::new("/usr/bin/ipcs").arg("-a").output().unwrap();
        let listing = String::from_utf8_lossy(&ipcs.stdout);
        let uid = uids[n].to_string();
        let left = listing.split_whitespace().any(|word| word == uid);
        assert!(
            !left,
            "an IPC object of uid {uid} is left after run {n}: {listing}"
        );
    }
    assert_eq!(
        uids[0], uids[10],
        "the eleventh run did not reuse the first uid: {uids:?}"
    );
    assert!(
        !owned_by(uids[0], &fixture.root),
        "files of the first uid are left"
    );
}

/// Asserts that a fork bomb is held at the process limit and that the stop still works: a
/// shell that starts sleeps without end reaches the limit of its account (64 processes and
/// the supervisor's), the stop ends every one, and the removal deletes the account. It
/// catches a stop that fails because the account has filled the process table.
#[test]
#[ignore = "needs root and creates users"]
fn a_fork_bomb_stops_at_the_process_limit_and_the_kill_still_works() {
    let fixture = fixture();
    let mut rt = runtime_with(fixture, |config| config.max_processes = 64);
    let script = "while :; do /bin/sleep 600 & done";
    let id = rt
        .create_and_start(&spec(fixture, "bomb", "/bin/sh", &["-c", script]))
        .unwrap();
    let home = home_of(fixture);
    let uid = fs::metadata(&home).unwrap().uid();
    let mut most = 0;
    for _ in 0..WATCHES {
        let count = process_uids().iter().filter(|one| **one == uid).count();
        most = most.max(count);
        if count >= 60 {
            break;
        }
        std::thread::yield_now();
    }
    assert!(most >= 60, "the bomb reached only {most} processes");
    assert!(most <= 65, "the limit did not hold: {most} processes");
    rt.stop(&id, Duration::ZERO).unwrap();
    settle(&mut rt, &id);
    rt.remove(&id).unwrap();
    assert!(
        !process_uids().contains(&uid),
        "a process of the account is left"
    );
    assert!(!home.exists(), "the home is left");
}

/// The sealer of the Mac the test runs on: trees `root:wheel` with `uchg`, writable copies
/// made by `clonefile`. The test runs as root, as the agent does.
#[cfg(target_os = "macos")]
fn mac_sealer() -> Arc<dyn Sealer> {
    use js_treestore::mac::{Clonefile, Fchflags, MacSealer, Owner};

    Arc::new(MacSealer::new(Owner::root(), Fchflags, Clonefile))
}

#[cfg(not(target_os = "macos"))]
fn mac_sealer() -> Arc<dyn Sealer> {
    panic!("these tests run on macOS only");
}

/// The machine's own bundle host, except that it does not check the directories above the store:
/// the agent's start requires each to be root's and closed to writes (`prepare_store`), and the
/// directory the operator names for these tests may lie below one that is not (a shared one
/// that others can write). The volume and securelevel checks, and everything else, are the
/// machine's own.
struct Unwalked(SystemHost<js_agent::SystemCommands>);

impl BundleHost for Unwalked {
    fn prepare_store(&self, _store: &Path) -> Result<(), String> {
        Ok(())
    }
    fn check_start(&self, store: &Path, scratch: &Path) -> Result<(), String> {
        self.0.check_start(store, scratch)
    }
    fn read_file(&self, path: &Path) -> Result<String, String> {
        self.0.read_file(path)
    }
    fn held(&self, store: &Path) -> Result<Vec<String>, String> {
        self.0.held(store)
    }
    fn resolve(&self, store: &Path, digest: &str) -> String {
        self.0.resolve(store, digest)
    }
    fn needs(&self, store: &Path, digest: &str) -> Result<Needs, String> {
        self.0.needs(store, digest)
    }
    fn os_version(&self) -> Result<js_bundle::DottedVersion, String> {
        self.0.os_version()
    }
    fn xcodes(&self) -> Vec<HostXcode> {
        self.0.xcodes()
    }
    fn used_bytes(&self, store: &Path) -> Result<u64, String> {
        self.0.used_bytes(store)
    }
    fn free_bytes(&self, path: &Path) -> Result<u64, String> {
        self.0.free_bytes(path)
    }
    fn can_run(&self) -> bool {
        self.0.can_run()
    }
    fn fetch(&self, store: &Path, name: &str, digest: &str) -> Fetch {
        self.0.fetch(store, name, digest)
    }
    fn open(&self, settings: &Settings) -> Result<Option<Arc<Placer>>, String> {
        self.0.open(settings)
    }
}

/// The store of a Mac agent below the fixture's directory, checked by the production
/// start checks and opened with the Mac's sealer, reaching registries through `client`.
/// The store directory is searchable by every user (mode 0711), as the slot accounts read
/// the trees in it.
fn mac_store(fixture: &Fixture, client: js_oci::Client) -> Arc<Placer> {
    let (store, scratch) = (
        fixture.root.join("bundle-store"),
        fixture.root.join("bundle-scratch"),
    );
    for (path, mode) in [(&store, 0o711), (&scratch, 0o700)] {
        fs::create_dir(path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }
    let (store, scratch) = (store.display().to_string(), scratch.display().to_string());
    let cli = Cli::try_parse_from([
        "js-agent",
        "--server",
        "http://127.0.0.1:1",
        "--pool",
        "macs",
        "--scratch-root",
        &scratch,
        "--bundle-store",
        &store,
        "--bundle-budget-gib",
        "10",
        "--bundle-trust",
        "digest",
    ])
    .unwrap();
    let mac = Platform {
        os: "macos".to_owned(),
        arch: "arm64".to_owned(),
    };
    let settings = Settings::from_cli(&cli, &mac, &Unwalked(SystemHost::system()))
        .unwrap()
        .unwrap();
    Placer::open(&settings, mac_sealer(), Arc::new(|| 1 << 40), client).unwrap()
}

/// A one-tree darwin/arm64 bundle whose entrypoint `show` records, in the directory it
/// runs in, the uid it runs as and the path it was started from. The tree is packed from
/// a directory below the fixture's.
fn show_bundle(fixture: &Fixture) -> Artifact {
    let tree = fixture.root.join("pack").join("app");
    fs::create_dir_all(tree.join("bin")).unwrap();
    let script = tree.join("bin").join("show");
    fs::write(
        &script,
        "#!/bin/sh\n/usr/bin/id -u > uid\necho \"$0\" > self\n",
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let packed = pack_layer(&tree).unwrap();
    let config = json!({
        "schema": 1,
        "os": "darwin",
        "architecture": "arm64",
        "os.version": "14.0",
        "name": "show",
        "version": "1",
        "layers": [{
            "tree": "app",
            "diff_id": packed.diff_id.as_str(),
            "uncompressed_bytes": packed.uncompressed_bytes,
            "entries": packed.entries,
            "license": "MIT",
            "source_url": "https://example.invalid/source",
        }],
        "env": [],
        "system_path": false,
        "entrypoints": {"show": {"path": "${tree:app}/bin/show"}},
        "writable_trees": [],
        "seatbelt": {"reads": [], "network_max": "off"},
        "built": {"deployment_target": "14.0", "tool": "tests 1"},
    });
    Artifact::bundle_with_config(vec![packed.blob], &config.to_string())
}

/// An image index of two entries, a linux/amd64 manifest and the darwin/arm64 `bundle`.
fn two_platform_index(linux: &Digest, bundle: &Artifact) -> Vec<u8> {
    let entry = |digest: &Digest, size: usize, os: &str, arch: &str| {
        json!({
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": digest.to_string(),
            "size": size,
            "platform": {"os": os, "architecture": arch},
        })
    };
    json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [
            entry(linux, 1, "linux", "amd64"),
            entry(&bundle.digest, bundle.manifest.len(), "darwin", "arm64"),
        ],
    })
    .to_string()
    .into_bytes()
}

/// Asserts that a Mac runs a job named by an image reference that carries the digest of an
/// image index: a registry on loopback in this process serves a two-entry index (linux/amd64
/// and darwin/arm64) and, for the darwin entry, a one-tree bundle; the store fetches the
/// reference by the index digest and takes the darwin/arm64 manifest from it, with no request
/// for the Linux entry; the start names the reference as its bundle and `@bundle/show` as
/// its program; the program runs from the store's tree as the run's account (the uid it
/// records is the uid of the account's home, in the pool's slots, not root); and the removal
/// deletes the account, which deletes its home. It catches an index digest that cannot be pulled, a
/// pull that takes the wrong platform's manifest, an entrypoint that runs as root or
/// unconfined, and a run directory left after the removal.
#[test]
#[ignore = "needs root and creates users"]
fn a_mac_runs_a_job_from_an_image_reference_by_digest() {
    let fixture = fixture();
    let tokio = tokio::runtime::Runtime::new().unwrap();
    let registry = tokio.block_on(Registry::start_plain());
    let bundle = show_bundle(fixture);
    let linux = Artifact::image(
        vec![b"x".to_vec()],
        r#"{"os":"linux","architecture":"amd64"}"#,
    );
    let index = two_platform_index(&linux.digest, &bundle);
    let index_digest = Digest::of(&index);
    registry.publish(&bundle);
    registry.publish_manifest_as(&index_digest, &index);

    let allow = Allow::parse(&format!("127.0.0.1:{}/team", registry.port))
        .unwrap()
        .loopback()
        .plain_http_loopback();
    let placer = mac_store(fixture, registry.client_with(vec![allow]));
    let name = format!("127.0.0.1:{}/team/bundle", registry.port);
    placer
        .ensure_registry(&name, &index_digest.to_string())
        .unwrap();
    let requests = registry.requests();
    assert!(
        requests.iter().any(|one| one.contains(bundle.digest.hex())),
        "the darwin manifest was not fetched: {requests:?}"
    );
    assert!(
        !requests.iter().any(|one| one.contains(linux.digest.hex())),
        "the linux manifest was asked for: {requests:?}"
    );

    let mut rt = runtime(fixture).with_bundles(Arc::clone(&placer));
    let mut run = spec(fixture, "image", "@bundle/show", &[]);
    run.bundle = Some(format!("{name}@{index_digest}"));
    let id = rt.create_and_start(&run).unwrap();
    assert_eq!(settle(&mut rt, &id), exited(0));
    let home = home_of(fixture);
    let uid = fs::metadata(&home).unwrap().uid();
    assert!(POOL_UIDS.contains(&uid), "ran as uid {uid}");
    let exec = home.join("exec");
    let recorded: u32 = fs::read_to_string(exec.join("uid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(
        recorded, uid,
        "the entrypoint did not run as the run's account"
    );
    let started_from = fs::read_to_string(exec.join("self")).unwrap();
    assert!(
        started_from.starts_with(&placer.root().display().to_string()),
        "the entrypoint did not start from the store: {started_from}"
    );
    rt.remove(&id).unwrap();
    assert!(!home.exists(), "the run directory is left");
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
