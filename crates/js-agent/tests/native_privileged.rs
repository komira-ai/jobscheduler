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
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use clap::Parser as _;
use js_agent::bundle::{Placer, Settings, SystemHost};
use js_agent::lifecycle::{Body, Exit, RunHandle, RunSpec, Runtime, Status};
use js_agent::native::{NativeConfig, NativeRuntime};
use js_agent::{Cli, Platform};
use js_agent_wire::{Isolation, NativeSpec, NetworkLevel};
use js_bundle::pack_layer;
use js_mac_pool::seatbelt::{AbsPath, Net, ReadModel};
use js_mac_session::client::Client;
use js_oci::{Allow, Digest};
use js_testkit::oci_registry::{Artifact, Fixture as Registry};
use js_treestore::Sealer;
use serde_json::json;

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
        for name in ["secret", "open"] {
            let folder = fixture.root.join(name);
            fs::create_dir(&folder).unwrap();
            fs::write(folder.join("f"), "x").unwrap();
        }
        serve(&fixture);
        fixture
    })
}

/// Serves the real helper on the fixture's socket, for as long as the process lives.
#[cfg(target_os = "macos")]
fn serve(fixture: &Fixture) {
    use std::os::unix::net::UnixStream;

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
    runtime_with(fixture, |_| {})
}

/// The production layout of the read denies: the state root and the scratch root (the runs'
/// standard-stream files are in it), and a directory of the test's own that file modes
/// leave open to everyone, so only the profile closes it.
fn runtime_with(fixture: &Fixture, adjust: impl FnOnce(&mut NativeConfig)) -> NativeRuntime {
    let denied = |path: &Path| AbsPath::new(path).unwrap();
    let mut config = NativeConfig {
        homes: fs::canonicalize(&fixture.homes).unwrap(),
        net: Net::Off,
        tool_roots: vec![PathBuf::from("/usr/bin"), PathBuf::from("/bin")],
        read_denies: vec![
            denied(&fixture.state),
            denied(&fixture.root.join("scratch")),
            denied(&fixture.root.join("secret")),
        ],
        read_model: ReadModel::Open,
        journal: fixture.root.join("native-leases"),
        max_file_bytes: 1 << 30,
        max_processes: 512,
        egress: None,
    };
    adjust(&mut config);
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
        labels: Default::default(),
        bundle: None,
        network: Default::default(),
        ports: Vec::new(),
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

/// Asserts a read deny that file modes do not explain, as a real lease user: the file in the
/// denied directory is world-readable (root made it, mode 0644, in a directory every user
/// may search), a file beside it outside the denied directory is read by the same user, and
/// the profile refuses the first. It also shows the production layout working: the scratch
/// root is denied and what the runs print still reaches their logs. It catches a read deny
/// that is only the mode of a directory.
#[test]
#[ignore = "needs root and creates users"]
fn a_read_deny_is_refused_to_a_real_user_the_modes_would_let_in() {
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

/// Asserts what a reused uid finds. The helper's uids are reused (ten runs on ten uids, so
/// the eleventh runs as the first did). The first run makes a System V shared memory segment
/// and leaves it: the profile does not deny IPC, so the segment is made, and the test
/// requires that once the user is deleted no IPC object belongs to its uid and no file on
/// the work volume does either. It fails, and says so, when the helper's removal leaves the
/// segment: that is the open risk, measured here. It catches a run that finds what an
/// earlier run on its uid left.
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
        uids.push(fs::metadata(home_of(fixture, &id)).unwrap().uid());
        rt.remove(&id).unwrap();
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
        "files of the first user are left"
    );
}

/// Asserts that a fork bomb is held at the process limit and that the stop still works: a
/// shell that starts sleeps without end reaches a limit of 64 processes of its user, the
/// helper's kill (which forks as root) ends every one, and the removal deletes the user.
/// It catches a stop that fails because the user has filled the process table.
#[test]
#[ignore = "needs root and creates users"]
fn a_fork_bomb_stops_at_the_process_limit_and_the_kill_still_works() {
    let fixture = fixture();
    let mut rt = runtime_with(fixture, |config| config.max_processes = 64);
    let script = "while :; do /bin/sleep 600 & done";
    let id = rt
        .create_and_start(&spec(fixture, "bomb", "/bin/sh", &["-c", script]))
        .unwrap();
    let uid = fs::metadata(home_of(fixture, &id)).unwrap().uid();
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
    assert!(most <= 64, "the limit did not hold: {most} processes");
    rt.stop(&id, Duration::from_secs(10)).unwrap();
    assert!(
        !process_uids().contains(&uid),
        "a process of the user is left"
    );
    rt.remove(&id).unwrap();
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

/// The store of a Mac agent below the fixture's directory, checked by the production
/// start checks and opened with the Mac's sealer, reaching registries through `client`.
/// The store directory is searchable by every user (mode 0711), as the lease users read
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
    let settings = Settings::from_cli(&cli, &mac, &SystemHost::system())
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
/// its program; the program runs from the store's tree as the lease user (the uid it records
/// is the uid of the user's home, in the helper's range, not root); and the removal deletes
/// the user and the run directory. It catches an index digest that cannot be pulled, a pull
/// that takes the wrong platform's manifest, an entrypoint that runs as root or unconfined,
/// and a run directory left after the removal.
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
    let home = home_of(fixture, &id);
    let uid = fs::metadata(&home).unwrap().uid();
    assert!((680..=689).contains(&uid), "ran as uid {uid}");
    let exec = home.join("exec");
    let recorded: u32 = fs::read_to_string(exec.join("uid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(
        recorded, uid,
        "the entrypoint did not run as the lease user"
    );
    let started_from = fs::read_to_string(exec.join("self")).unwrap();
    assert!(
        started_from.starts_with(&placer.root().display().to_string()),
        "the entrypoint did not start from the store: {started_from}"
    );
    rt.remove(&id).unwrap();
    assert!(!home.exists(), "the run directory is left");
}
