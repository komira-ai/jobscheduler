//! The whole agent with native jobs: where the flags are accepted, what they require, and
//! what the hello says. The helper is a fake on a socket and the runtime is the real one.

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::sync::Arc;

use clap::Parser as _;
use js_agent::lifecycle::Local;
use js_agent::native::{self, NativeFlags, NativeSettings, PfFlags};
use js_agent::{Backend, Cli, Machine, RuntimeBuild, System};
use serde_json::Value;
use std::path::Path;

use super::fake_helper::FakeHelper;
use crate::bundle::fake::{FakeHost, digest};
use crate::bundle::fixture::{no_registry, placer};
use crate::jobs::fake::FakeMachine;
use crate::support::{
    CountingSleeper, Host, Jobs, Lowest, Ran, Scratch, Script, ScriptConnector, code, flags,
    mac_host, run_on, run_on_defaulted, run_with_store, welcome,
};

/// A private directory `name` under `dir`.
pub(crate) fn private(dir: &Scratch, name: &str) -> String {
    let path = dir.path(name);
    fs::create_dir(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    path.display().to_string()
}

/// The flags of a native agent over `fake` with its state and scratch roots in `dir`.
pub(crate) fn native_flags(dir: &Scratch, fake: &FakeHelper) -> Vec<String> {
    vec![
        "--native-helper-socket".to_owned(),
        fake.socket().display().to_string(),
        "--native-homes".to_owned(),
        fake.homes().display().to_string(),
        "--state-root".to_owned(),
        private(dir, "state"),
        "--scratch-root".to_owned(),
        private(dir, "scratch"),
    ]
}

/// Runs the whole agent on `host` with `extra` flags until the server's first answer
/// (`answer`); returns what it printed and the hello it sent (`null` for none).
pub(crate) fn hello_on(
    host: &Host,
    extra: &[String],
    answer: crate::support::Reply,
) -> (Ran, Value) {
    let refs: Vec<&str> = extra.iter().map(String::as_str).collect();
    let args = flags(&refs);
    let (script, sent) = Script::new(vec![answer]);
    let connector = ScriptConnector::of(script);
    let mut sleeper = CountingSleeper::stopping_at(1);
    let machine = Arc::new(FakeMachine::new());
    let jobs: Jobs<'_> = (machine, None);
    let ran = run_on(&args, host, &connector, &mut sleeper, &mut Lowest, jobs);
    let hello = sent
        .borrow()
        .first()
        .map_or(Value::Null, |sent| sent.1.clone());
    (ran, hello)
}

/// Asserts that a Mac agent given the helper's socket starts its worker (the helper is
/// asked for its boot reset first) and reports `runtime` and `exec` as `sandbox-exec`
/// beside `os` and `arch`. It catches an agent that runs native jobs while reporting no
/// mode (every pool with a mode would refuse it).
#[test]
fn a_mac_agent_reports_sandbox_exec() {
    let dir = Scratch::new("native-agent-hello");
    let fake = FakeHelper::start(&dir);
    let extra = native_flags(&dir, &fake);
    let (ran, hello) = hello_on(&mac_host(), &extra, welcome(1));
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    assert_eq!(fake.verbs(), ["boot-reset"]);
    assert_eq!(hello["capabilities"]["runtime"], "sandbox-exec");
    assert_eq!(hello["capabilities"]["exec"], "sandbox-exec");
    assert_eq!(hello["capabilities"]["os"], "macos");
}

/// Asserts that a Mac start with neither `--native-helper-socket` nor `--no-jobs` ends with
/// status 2, before anything is sent, naming `--native-helper-socket` and the `--no-jobs` that
/// an agent running no job gives. It catches a Mac agent that registers with no way to run a
/// job, and one that is silently a no-job agent for want of a flag.
#[test]
fn a_mac_start_with_no_socket_and_no_no_jobs_is_refused() {
    let args = flags(&[]);
    let (script, sent) = Script::new(vec![welcome(1)]);
    let connector = ScriptConnector::of(script);
    let mut sleeper = CountingSleeper::stopping_at(1);
    let jobs: Jobs<'_> = (Arc::new(FakeMachine::new()), None);
    let ran = run_on_defaulted(
        &args,
        &mac_host(),
        &connector,
        &mut sleeper,
        &mut Lowest,
        jobs,
    );
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    assert!(
        ran.err.contains(
            "--native-helper-socket: required: this macOS build runs native jobs through the \
             root helper (give --no-jobs for an agent that runs no job)"
        ),
        "{}",
        ran.err
    );
    assert!(sent.borrow().is_empty(), "something was sent");
}

/// Asserts that `--no-jobs` refuses each flag of the macOS build's runtime, the native socket
/// first among them: through the agent's own entry point, a start with `--no-jobs` and one such
/// flag ends with status 2 before anything is sent, naming the flag and `--no-jobs`. It catches
/// an agent that runs no job yet takes a runtime flag it would ignore, so that a flags file
/// written for a native agent starts one that silently runs nothing.
#[test]
fn no_jobs_refuses_every_native_flag_by_name() {
    let rows: [(&str, &str); 5] = [
        ("--native-helper-socket", "/var/run/js-mac-session/socket"),
        ("--native-network", "on"),
        ("--native-pf-uid-start", "600"),
        ("--vm-driver", "/usr/local/bin/js-vmm"),
        ("--vm-store", "/var/db/js-vm"),
    ];
    for (flag, value) in rows {
        let args = flags(&["--no-jobs", flag, value]);
        let (script, sent) = Script::new(vec![welcome(1)]);
        let connector = ScriptConnector::of(script);
        let mut sleeper = CountingSleeper::stopping_at(1);
        let jobs: Jobs<'_> = (Arc::new(FakeMachine::new()), None);
        let ran = run_on_defaulted(
            &args,
            &mac_host(),
            &connector,
            &mut sleeper,
            &mut Lowest,
            jobs,
        );
        assert_eq!(ran.status(), code(2), "{flag}: {}", ran.err);
        assert_eq!(
            ran.err,
            format!("js-agent: {flag}: not with --no-jobs (an agent that runs no job)\n"),
            "{flag}"
        );
        assert!(sent.borrow().is_empty(), "{flag}: something was sent");
    }
}

/// Asserts that a Mac agent's hello reports its `--native-network` as the job level a
/// server places by: `none` by default (`off`), `none` for `loopback`, `open` for `open`, and
/// never `egress-ports`. It catches a Mac host that a server would send an `open` job it
/// refuses, or one that says nothing and is never sent one.
#[test]
fn a_mac_agent_advertises_its_native_network_ceiling() {
    for (n, (given, reported)) in [
        (None, "none"),
        (Some("loopback"), "none"),
        (Some("open"), "open"),
    ]
    .into_iter()
    .enumerate()
    {
        let dir = Scratch::new(&format!("native-agent-egress{n}"));
        let fake = FakeHelper::start(&dir);
        let mut extra = native_flags(&dir, &fake);
        if let Some(value) = given {
            extra.extend(["--native-network".to_owned(), value.to_owned()]);
        }
        let (ran, hello) = hello_on(&mac_host(), &extra, welcome(1));
        assert_eq!(ran.status(), code(0), "{given:?}: {}", ran.err);
        assert_eq!(hello["capabilities"]["egress"], reported, "{given:?}");
        assert!(
            hello["capabilities"].get("egress-ports").is_none(),
            "{given:?}"
        );
    }
}

/// Asserts that an agent whose helper does not answer ends with status 2 before it says
/// hello, naming the flag. It catches an agent that registers as a native host with no way
/// to run a job.
#[test]
fn an_agent_whose_helper_is_not_there_ends_before_the_hello() {
    let dir = Scratch::new("native-agent-nohelper");
    let fake = FakeHelper::start(&dir);
    let mut extra = native_flags(&dir, &fake);
    extra[1] = "/nonexistent/js-helper".to_owned();
    let (ran, hello) = hello_on(&mac_host(), &extra, welcome(1));
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    assert!(ran.err.contains("--native-helper-socket"), "{}", ran.err);
    assert_eq!(hello, Value::Null);
}

/// Asserts what the job flags require of a native agent: a state root and a scratch root
/// (the taint marker and each run's logs are kept there) and a `--stop-grace` that is
/// accepted. It catches a native agent that starts with no place for its taint marker.
#[test]
fn a_native_agent_needs_its_roots() {
    let dir = Scratch::new("native-agent-roots");
    let fake = FakeHelper::start(&dir);
    let socket = fake.socket().display().to_string();
    let homes = fake.homes().display().to_string();
    let (state, scratch) = (private(&dir, "state"), private(&dir, "scratch"));
    let base = ["--native-helper-socket", &socket, "--native-homes", &homes];
    let cli = |extra: &[&str]| {
        let all = [&base[..], extra].concat();
        let argv = [
            "js-agent",
            "--server",
            "http://127.0.0.1:1",
            "--pool",
            "macs",
        ];
        Cli::try_parse_from(argv.iter().chain(all.iter())).unwrap()
    };

    let why = Local::from_cli(&cli(&["--scratch-root", &scratch]), 0).unwrap_err();
    assert!(
        why.starts_with("--state-root: required with --native-helper-socket"),
        "{why}"
    );
    let why = Local::from_cli(&cli(&["--state-root", &state]), 0).unwrap_err();
    assert!(
        why.starts_with("--scratch-root: required with --native-helper-socket"),
        "{why}"
    );
    let both = ["--state-root", &state, "--scratch-root", &scratch];
    let local = Local::from_cli(&cli(&both), 0).unwrap();
    assert!(matches!(local.backend, Some(Backend::Native(_))));
    let graced = [&both[..], &["--stop-grace", "5"]].concat();
    let local = Local::from_cli(&cli(&graced), 0).unwrap();
    assert_eq!(local.stop_grace.as_secs(), 5);
}

/// Asserts that the system machine makes the native runtime for the native backend: over a
/// helper that answers it asks for the boot reset, and over a socket nobody listens on it
/// fails with the flag named. It catches a native agent that falls back to an engine, and
/// one that starts with a helper that is not there.
#[test]
fn the_system_machine_opens_the_native_runtime() {
    let dir = Scratch::new("native-agent-system");
    let fake = FakeHelper::start(&dir);
    let platform = js_agent::Platform {
        os: "macos".to_owned(),
        arch: "aarch64".to_owned(),
    };
    let open = |socket: &std::path::Path, sandbox: &str| {
        let flags = NativeFlags {
            socket: Some(socket),
            homes: Some(fake.homes()),
            network: None,
            tool_roots: &[],
            deny_read: &[],
            max_file_mib: None,
            max_processes: None,
            pf: PfFlags::default(),
        };
        let settings = NativeSettings::from_flags(&flags, &[], Path::new("/srv/journal"))
            .unwrap()
            .unwrap();
        let backend = Backend::Native(settings.with_sandbox_exec(Path::new(sandbox)));
        let build = RuntimeBuild {
            backend: &backend,
            host_id: "h1",
            platform: &platform,
            stop_grace: std::time::Duration::from_secs(1),
        };
        System::new().runtime(&build).map(|(_, warnings)| warnings)
    };
    assert_eq!(open(fake.socket(), "/usr/bin/true"), Ok(Vec::new()));
    assert_eq!(fake.verbs(), ["boot-reset"]);
    let why = open(
        std::path::Path::new("/nonexistent/js-helper"),
        "/usr/bin/true",
    )
    .unwrap_err();
    assert!(
        why.starts_with("--native-helper-socket: the boot reset: "),
        "{why}"
    );
    let before = fake.verbs().len();
    let why = open(fake.socket(), "/usr/bin/false").unwrap_err();
    assert!(why.contains("the sandbox self-test failed"), "{why}");
    assert_eq!(fake.verbs().len(), before, "the helper was not asked");
}

/// Asserts that a native agent that is not root is refused at start, naming the flag, and
/// that the agent's token, instance and CA files, the helper's socket directory and
/// `--native-deny-read` are read-denied beside its roots. It catches an agent that takes
/// leases and fails every start at the first `chown`, and a secret file a run can read.
#[test]
fn a_native_agent_must_be_root_and_denies_its_secret_files() {
    let dir = Scratch::new("native-agent-euid");
    let fake = FakeHelper::start(&dir);
    let socket = fake.socket().display().to_string();
    let homes = fake.homes().display().to_string();
    let (state, scratch) = (private(&dir, "state"), private(&dir, "scratch"));
    let files: Vec<String> = ["token", "instance", "ca.pem"]
        .iter()
        .map(|name| dir.write(name, "x", 0o600).display().to_string())
        .collect();
    let argv = [
        "js-agent",
        "--server",
        "http://127.0.0.1:1",
        "--pool",
        "macs",
        "--token-file",
        &files[0],
        "--instance-file",
        &files[1],
        "--ca-cert",
        &files[2],
        "--native-helper-socket",
        &socket,
        "--native-homes",
        &homes,
        "--state-root",
        &state,
        "--scratch-root",
        &scratch,
    ];
    let cli = Cli::try_parse_from(argv).unwrap();
    let why = Local::from_cli(&cli, 501).unwrap_err();
    assert!(
        why.starts_with("--native-helper-socket: the agent must run as root"),
        "{why}"
    );
    let local = Local::from_cli(&cli, 0).unwrap();
    let native = local.backend.unwrap();
    let native = native.settings();
    let denied: Vec<&str> = native
        .config()
        .read_denies
        .iter()
        .map(|d| d.as_str())
        .collect();
    for path in [&state, &scratch, &files[0], &files[1], &files[2]] {
        let resolved = fs::canonicalize(path).unwrap();
        assert!(
            denied.contains(&resolved.to_str().unwrap()),
            "{path} is not denied: {denied:?}"
        );
    }
    assert!(denied.contains(&"/private/etc/kcpassword"));
}

/// Asserts the start-up self-test of the sandbox: a program that runs and accepts the
/// profile passes; one that refuses it fails with its status and what it said; one that
/// cannot be run fails naming it; a configuration with no profile builder fails by name.
/// It catches an agent that says `sandbox-exec` while every job it takes would fail.
#[test]
fn the_sandbox_self_test_fails_closed() {
    let dir = Scratch::new("native-selftest");
    let fake = FakeHelper::start(&dir);
    let flags = NativeFlags {
        socket: Some(fake.socket()),
        homes: Some(fake.homes()),
        network: None,
        tool_roots: &[],
        deny_read: &[],
        max_file_mib: None,
        max_processes: None,
        pf: PfFlags::default(),
    };
    let settings = NativeSettings::from_flags(&flags, &[], Path::new("/srv/journal"))
        .unwrap()
        .unwrap();
    let config = settings.config();
    assert_eq!(
        native::self_test(config, Path::new("/usr/bin/true")),
        Ok(())
    );
    let refused = native::self_test(config, Path::new("/usr/bin/false")).unwrap_err();
    assert!(
        refused.contains("/usr/bin/false refused the profile"),
        "{refused}"
    );
    let missing = native::self_test(config, Path::new("/nonexistent/sandbox-exec")).unwrap_err();
    assert!(
        missing.starts_with("/nonexistent/sandbox-exec: "),
        "{missing}"
    );
    let mut allow = config.clone();
    allow.read_model = js_mac_pool::seatbelt::ReadModel::Allow;
    let unbuilt = native::self_test(&allow, Path::new("/usr/bin/true")).unwrap_err();
    assert!(unbuilt.starts_with("the profile: "), "{unbuilt}");
}

/// The flags of a Mac agent that runs native jobs over `fake` and keeps a store of bundles.
fn store_flags(dir: &Scratch, fake: &FakeHelper, extra: &[&str]) -> Vec<String> {
    let mut flags = native_flags(dir, fake);
    let own = [
        "--bundle-store",
        "/Volumes/work/bundles",
        "--bundle-budget-gib",
        "10",
        "--bundle-trust",
        "digest",
    ];
    flags.extend(own.iter().chain(extra).map(|flag| (*flag).to_owned()));
    flags
}

/// Runs the whole agent on a Mac with `extra` flags and the bundle host `store`, until the
/// server's first answer.
fn agent_with_store(
    dir: &Scratch,
    extra: &[String],
    store: Arc<FakeHost>,
    machine: &Arc<FakeMachine>,
) -> (Ran, Value) {
    let mut host = mac_host();
    let row =
        "Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/disk3 9 9 99999999 1% /\n";
    let scratch = dir.path("scratch");
    host.commands = host
        .commands
        .with(&format!("/bin/df -P -k {}", scratch.display()), Ok(row));
    let refs: Vec<&str> = extra.iter().map(String::as_str).collect();
    let (script, sent) = Script::new(vec![welcome(1)]);
    let connector = ScriptConnector::of(script);
    let mut sleeper = CountingSleeper::stopping_at(1);
    let jobs: Jobs<'_> = (machine.clone(), None);
    let args = flags(&refs);
    let ran = run_with_store(
        &args,
        &host,
        &connector,
        &mut sleeper,
        &mut Lowest,
        jobs,
        store,
    );
    let hello = sent
        .borrow()
        .first()
        .map_or(Value::Null, |sent| sent.1.clone());
    (ran, hello)
}

/// Asserts that a Mac agent that runs native jobs and keeps a store opens the store before
/// it says hello, hands it to the runtime it makes, and reports the bundle it holds; and that
/// an agent with a store and no native jobs opens nothing, because it could never run a
/// bundle. It catches a runtime that has no store while the hello says it has one, and a
/// store opened by an agent that cannot use it.
#[test]
fn a_native_agent_opens_its_store_and_gives_it_to_the_runtime() {
    let dir = Scratch::new("native-agent-store");
    let fake = FakeHelper::start(&dir);
    let bundles = Scratch::new("native-agent-store-placer");
    let store = FakeHost::new();
    store.set(|s| {
        s.open = Ok(Some(placer(&bundles, no_registry())));
        s.held = Ok(vec![digest('a')]);
    });
    let machine = Arc::new(FakeMachine::new());
    let extra = store_flags(&dir, &fake, &[]);
    let (ran, hello) = agent_with_store(&dir, &extra, store.clone(), &machine);
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    assert_eq!(store.get(|s| s.opens), 1);
    assert_eq!(machine.bundles_seen(), [true]);
    assert_eq!(hello["capabilities"]["bundle"], digest('a'));
    assert_eq!(
        hello["capabilities"]["bundle-root"],
        "/Volumes/work/bundles"
    );

    let other = Scratch::new("native-agent-store-none");
    let helper = FakeHelper::start(&other);
    let store = FakeHost::new();
    let machine = Arc::new(FakeMachine::new());
    let mut flags = store_flags(&other, &helper, &[]);
    flags.drain(
        ..flags
            .iter()
            .position(|flag| flag == "--bundle-store")
            .unwrap(),
    );
    // The native flags are gone, and the scratch root they named is still there.
    flags.extend([
        "--scratch-root".to_owned(),
        other.path("scratch").display().to_string(),
    ]);
    let (ran, _) = agent_with_store(&other, &flags, store.clone(), &machine);
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    assert_eq!(store.get(|s| s.opens), 0, "no native jobs, no store");
    assert!(machine.bundles_seen().is_empty());
}

/// Asserts that a store that cannot be opened ends the agent with status 2 before the hello,
/// naming the flag, and that a store in a directory no native run may read (here one the
/// operator names with `--native-deny-read`) does too: every bundle job would fail at its
/// first read. It catches an agent that registers and then fails every bundle job.
#[test]
fn a_store_that_cannot_be_used_ends_the_agent_before_the_hello() {
    let dir = Scratch::new("native-agent-badstore");
    let fake = FakeHelper::start(&dir);
    let bundles = Scratch::new("native-agent-badstore-placer");
    let store = FakeHost::new();
    store.set(|s| s.open = Err("cannot open it".to_owned()));
    let machine = Arc::new(FakeMachine::new());
    let extra = store_flags(&dir, &fake, &[]);
    let (ran, hello) = agent_with_store(&dir, &extra, store, &machine);
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    assert!(
        ran.err.contains("--bundle-store: cannot open it"),
        "{}",
        ran.err
    );
    assert_eq!(hello, Value::Null);

    let opened = placer(&bundles, no_registry());
    let denied = opened.root().display().to_string();
    let store = FakeHost::new();
    store.set(|s| s.open = Ok(Some(opened)));
    let machine = Arc::new(FakeMachine::new());
    let mut extra = extra;
    extra.extend(["--native-deny-read".to_owned(), denied]);
    let (ran, hello) = agent_with_store(&dir, &extra, store, &machine);
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    assert!(ran.err.contains("--bundle-store"), "{}", ran.err);
    assert!(ran.err.contains("may read"), "{}", ran.err);
    assert_eq!(hello, Value::Null);
}

/// Asserts that the system machine hands the store of the native settings to the runtime it
/// makes: the runtime accepts a program of a bundle with a store and refuses it
/// (`bundle-unsupported`) without. It catches a store that is opened and never used.
#[test]
fn the_system_machine_gives_the_store_to_the_native_runtime() {
    let dir = Scratch::new("native-agent-system-store");
    let fake = FakeHelper::start(&dir);
    let bundles = Scratch::new("native-agent-system-store-placer");
    let platform = js_agent::Platform {
        os: "macos".to_owned(),
        arch: "aarch64".to_owned(),
    };
    let flags = NativeFlags {
        socket: Some(fake.socket()),
        homes: Some(fake.homes()),
        network: None,
        tool_roots: &[],
        deny_read: &[],
        max_file_mib: None,
        max_processes: None,
        pf: PfFlags::default(),
    };
    let plain = NativeSettings::from_flags(&flags, &[], Path::new("/srv/journal"))
        .unwrap()
        .unwrap();
    let plain = plain.with_sandbox_exec(Path::new("/usr/bin/true"));
    let stored = plain
        .clone()
        .with_bundles(placer(&bundles, no_registry()))
        .unwrap();
    assert!(stored.bundles().is_some() && plain.bundles().is_none());
    let program = super::native("@bundle/tool", &[]);
    let codes: Vec<Option<&str>> = [plain, stored]
        .into_iter()
        .map(|settings| {
            let backend = Backend::Native(settings);
            let build = RuntimeBuild {
                backend: &backend,
                host_id: "h1",
                platform: &platform,
                stop_grace: std::time::Duration::from_secs(1),
            };
            let (runtime, _) = System::new().runtime(&build).unwrap();
            runtime
                .check_native(&program)
                .err()
                .map(|refused| refused.code)
        })
        .collect();
    assert_eq!(codes, [Some("bundle-unsupported"), None]);
}

/// Asserts that an agent whose cap is `on` ends with status 2 before the hello when it is
/// given no pool uids (the flag named), and when the anchor is not enforced (here the
/// `pfctl` it is told is not there), naming `--native-network` and the failed call; and that
/// the pf flags are accepted by the command line of an agent that is told them. It catches
/// an agent that says hello with the network on and nothing limiting it.
#[test]
fn an_agent_whose_cap_is_on_does_not_start_while_the_anchor_is_not_enforced() {
    let dir = Scratch::new("native-agent-pf");
    let fake = FakeHelper::start(&dir);
    let mut extra = native_flags(&dir, &fake);
    extra.extend(["--native-network".to_owned(), "on".to_owned()]);
    let (ran, hello) = hello_on(&mac_host(), &extra, welcome(1));
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    assert!(ran.err.contains("--native-pf-uid-start"), "{}", ran.err);
    assert_eq!(hello, Value::Null);

    let pool = [
        "--native-pf-uid-start",
        "600",
        "--native-pf-uid-count",
        "10",
        "--native-pf-endpoint",
        "192.0.2.10:443",
        "--native-pf-anchor",
        "com.apple/pool",
        "--native-pf-state-dir",
        "/nonexistent/pf-state",
        "--native-pfctl",
        "/nonexistent/pfctl",
    ];
    extra.extend(pool.iter().map(|flag| (*flag).to_owned()));
    let (ran, hello) = hello_on(&mac_host(), &extra, welcome(1));
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    assert!(ran.err.contains("--native-network"), "{}", ran.err);
    assert!(ran.err.contains("could not run pfctl"), "{}", ran.err);
    assert_eq!(hello, Value::Null);
}

/// A property list of one dictionary with `body`, as `diskutil -plist` prints it.
fn plist(body: &str) -> String {
    format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict>{body}</dict></plist>\n")
}

/// Asserts that a Mac agent whose scratch root is on its state root's file system (as after a
/// boot, which mounts no scratch volume) asks `diskutil` to mount the scratch volume on the
/// scratch root before it opens either root, and that one whose mount fails ends before the
/// hello naming `--scratch-root` and the command. Catches an agent that never mounts the
/// volume, which would refuse to start after every reboot.
#[test]
fn a_mac_agent_mounts_the_scratch_volume_before_it_opens_its_roots() {
    let dir = Scratch::new("native-agent-scratch-volume");
    let fake = FakeHelper::start(&dir);
    let extra = native_flags(&dir, &fake);
    let scratch = dir.path("scratch").display().to_string();
    let data = "/usr/sbin/diskutil info -plist /System/Volumes/Data";
    let list = "/usr/sbin/diskutil apfs list -plist disk3";
    let info = "/usr/sbin/diskutil info -plist disk3s7";
    let mount = format!("/usr/sbin/diskutil mount nobrowse -mountPoint {scratch} disk3s7");
    let container = plist(
        "<key>Containers</key><array><dict><key>CapacityFree</key><integer>1000</integer>\
         <key>ContainerReference</key><string>disk3</string><key>Volumes</key><array><dict>\
         <key>DeviceIdentifier</key><string>disk3s7</string>\
         <key>Name</key><string>js-agent-scratch</string></dict></array></dict></array>",
    );
    let mut host = mac_host();
    host.files = host.files.with_device_under(&dir.path(""), 7);
    host.commands = host
        .commands
        .with(
            data,
            Ok(&plist(
                "<key>APFSContainerReference</key><string>disk3</string>",
            )),
        )
        .with(list, Ok(&container))
        .with(info, Ok(&plist("<key>MountPoint</key><string></string>")));
    let (ran, hello) = hello_on(&host, &extra, welcome(1));
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    assert!(
        ran.err.contains(&format!("--scratch-root: {mount}: ")),
        "{}",
        ran.err
    );
    assert_eq!(hello, Value::Null);
    let ran_commands = host.commands.ran.borrow().clone();
    let at = |line: &str| ran_commands.iter().position(|ran| ran == line);
    assert!(at(data) < at(list) && at(list) < at(info), "{ran_commands:?}");
    assert_eq!(ran_commands.last(), Some(&mount), "{ran_commands:?}");
}
