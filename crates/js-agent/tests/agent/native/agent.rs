//! The whole agent with native jobs: where the flags are accepted, what they require, and
//! what the hello says. The test runner holds no Full Disk Access, so the agent opens no
//! account pool and runs no job here (the pool's runs are tested at the runtime, in the
//! other modules); what these tests read is the agent's start: its flags, its refusals before
//! the hello, and the hello.

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::sync::Arc;

use clap::Parser as _;
use js_agent::lifecycle::Local;
use js_agent::native::fence::PRIVACY_ONLY_PLACES;
use js_agent::native::{NativeFlags, NativeSettings, PfFlags};
use js_agent::{Backend, Cli, Machine, RuntimeBuild, System};
use js_agent_wire::refusal;
use js_caps::FULL_DISK_ACCESS;
use js_mac_session::full_disk_access::FullDiskAccess;
use serde_json::Value;
use std::path::Path;

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

/// The uids of the account pool of the agents made here.
const ACCOUNT_RANGE: &str = "501-510";

/// The flags of a native agent with its state and scratch roots in `dir`, and the account pool
/// `dir/accounts` (a directory the agent makes nothing in while it holds no Full Disk Access)
/// of ten slots.
pub(crate) fn native_flags(dir: &Scratch) -> Vec<String> {
    vec![
        "--native-accounts".to_owned(),
        dir.path("accounts").display().to_string(),
        "--native-account-range".to_owned(),
        ACCOUNT_RANGE.to_owned(),
        "--state-root".to_owned(),
        private(dir, "state"),
        "--scratch-root".to_owned(),
        private(dir, "scratch"),
    ]
}

/// `flags` without `flag` and the value that follows it.
fn without(flags: &[String], flag: &str) -> Vec<String> {
    let at = flags.iter().position(|given| given == flag).unwrap();
    let mut rest = flags.to_vec();
    rest.drain(at..=at + 1);
    rest
}

/// The native settings of an agent with the account pool `/var/db/js-accounts` that found
/// `held` at start.
fn settings(held: FullDiskAccess) -> NativeSettings {
    let flags = NativeFlags {
        accounts: Some(Path::new("/var/db/js-accounts")),
        account_range: Some(ACCOUNT_RANGE),
        account_ready: None,
        homes: None,
        network: None,
        tool_roots: &[],
        deny_read: &[],
        max_file_mib: None,
        max_processes: None,
        pf: PfFlags::default(),
    };
    NativeSettings::from_flags(&flags, &[], Path::new("/srv/journal"))
        .unwrap()
        .unwrap()
        .with_full_disk_access(held)
}

/// The native backend of an agent with its flags checked and its journal in `dir`, for a worker
/// (`native/worker.rs`) or a rescue (`vm/rescue.rs`) over a machine that makes the runtime. It
/// has not found Full Disk Access.
pub(crate) fn backend(dir: &Scratch) -> Backend {
    let accounts = dir.path("accounts");
    let flags = NativeFlags {
        accounts: Some(&accounts),
        account_range: Some(ACCOUNT_RANGE),
        account_ready: None,
        homes: None,
        network: None,
        tool_roots: &[],
        deny_read: &[],
        max_file_mib: None,
        max_processes: None,
        pf: PfFlags::default(),
    };
    let journal = dir.path("native-leases");
    let found = NativeSettings::from_flags(&flags, &[], &journal);
    Backend::Native(found.unwrap().unwrap())
}

/// Runs the whole agent on `host` with `extra` flags until the server's first answer
/// (`answer`); returns what it printed and the hello it sent (`null` for none).
pub(crate) fn hello_on(
    host: &Host,
    extra: &[String],
    answer: crate::support::Reply,
) -> (Ran, Value) {
    let refs: Vec<&str> = extra.iter().map(String::as_str).collect();
    hello_with(host, &refs, answer, FakeMachine::new())
}

/// [`hello_on`] over `machine`, with the flags `refs`.
pub(crate) fn hello_with(
    host: &Host,
    refs: &[&str],
    answer: crate::support::Reply,
    machine: FakeMachine,
) -> (Ran, Value) {
    let args = flags(refs);
    let (script, sent) = Script::new(vec![answer]);
    let connector = ScriptConnector::of(script);
    let mut sleeper = CountingSleeper::stopping_at(1);
    let jobs: Jobs<'_> = (Arc::new(machine), None);
    let ran = run_on(&args, host, &connector, &mut sleeper, &mut Lowest, jobs);
    let hello = sent
        .borrow()
        .first()
        .map_or(Value::Null, |sent| sent.1.clone());
    (ran, hello)
}

/// Asserts that a Mac agent given its account pool starts its worker and reports `runtime` and
/// `exec` as `sandbox-exec` beside `os`, and the Full Disk Access it found (the test runner
/// holds none, so it says `not-granted` or `undetermined`, never `granted`), and that with no
/// grant it opens no pool: nothing is made in the pool's directory. It catches an agent that
/// runs native jobs while reporting no mode (every pool with a mode would refuse it), one that
/// does not say what it found, and one that opens its pool without the grant that deletes an
/// account.
#[test]
fn a_mac_agent_reports_sandbox_exec() {
    let dir = Scratch::new("native-agent-hello");
    let extra = native_flags(&dir);
    let (ran, hello) = hello_on(&mac_host(), &extra, welcome(1));
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    assert_eq!(hello["capabilities"]["runtime"], "sandbox-exec");
    assert_eq!(hello["capabilities"]["exec"], "sandbox-exec");
    assert_eq!(hello["capabilities"]["os"], "macos");
    let held = hello["capabilities"][FULL_DISK_ACCESS].as_str().unwrap();
    assert!(["not-granted", "undetermined"].contains(&held), "{held}");
    assert!(!dir.path("accounts").exists(), "no pool without the grant");
}

/// Asserts that a Mac start with neither `--native-accounts` nor `--no-jobs` ends with
/// status 2, before anything is sent, naming `--native-accounts` and the `--no-jobs` that
/// an agent running no job gives. It catches a Mac agent that registers with no way to run a
/// job, and one that is silently a no-job agent for want of a flag.
#[test]
fn a_mac_start_with_no_accounts_and_no_no_jobs_is_refused() {
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
            "--native-accounts: required: this macOS build runs native jobs as accounts of its \
             own pool (give --no-jobs for an agent that runs no job)"
        ),
        "{}",
        ran.err
    );
    assert!(sent.borrow().is_empty(), "something was sent");
}

/// Asserts that `--no-jobs` refuses each flag of the macOS build's runtime, the native
/// accounts and their range first among them: through the agent's own entry point, a start with `--no-jobs` and one such
/// flag ends with status 2 before anything is sent, naming the flag and `--no-jobs`. It catches
/// an agent that runs no job yet takes a runtime flag it would ignore, so that a flags file
/// written for a native agent starts one that silently runs nothing.
#[test]
fn no_jobs_refuses_every_native_flag_by_name() {
    let rows: [(&str, &str); 6] = [
        ("--native-accounts", "/var/db/js-accounts"),
        ("--native-account-range", "501-510"),
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
        let mut extra = native_flags(&dir);
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

/// Asserts the provider flags through the whole macOS agent: given `--mount-provider`,
/// `--mount-provider-user` naming a user of the directory service (`nobody`, on every Mac) and
/// `--mount-provider-wait`, the agent starts and its hello reports `mount-provider.tierstore`
/// beside `layered-rootfs`; without them it reports no provider; a user the directory service
/// does not know ends the agent with status 2 before anything is sent, naming the flag; and
/// `--no-jobs` refuses each of the three flags, and `--native-trees-root`, by name. Catches a Mac agent that drops the
/// flags, one never placed a provider's job, a provider's socket judged against root for a
/// name the agent could not look up, and a no-job agent that silently ignores the flags.
#[test]
fn a_mac_agent_takes_the_provider_flags_and_reports_the_provider() {
    let dir = Scratch::new("native-agent-providers");
    let socket = dir.path("provider").join("provider.sock");
    fs::create_dir(dir.path("provider")).unwrap();
    let provider = format!("tierstore={}", socket.display());
    let with = |user: &str| {
        let mut extra = native_flags(&dir);
        extra.extend(
            [
                "--mount-provider",
                &provider,
                "--mount-provider-user",
                &format!("tierstore={user}"),
                "--mount-provider-wait",
                "60",
            ]
            .map(str::to_owned),
        );
        extra
    };
    let (ran, hello) = hello_on(&mac_host(), &with("nobody"), welcome(1));
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    assert_eq!(hello["capabilities"]["mount-provider.tierstore"], "true");
    assert_eq!(hello["capabilities"]["layered-rootfs"], "true");

    let bare = Scratch::new("native-agent-providers-none");
    let (ran, hello) = hello_on(&mac_host(), &native_flags(&bare), welcome(1));
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    let keys = hello["capabilities"].as_object().unwrap();
    assert!(
        !keys.keys().any(|key| key.starts_with("mount-provider.")),
        "{hello}"
    );

    let other = Scratch::new("native-agent-providers-unknown");
    let mut extra = native_flags(&other);
    extra.extend(
        [
            "--mount-provider",
            &provider,
            "--mount-provider-user",
            "tierstore=_js-no-such-user",
        ]
        .map(str::to_owned),
    );
    let (ran, hello) = hello_on(&mac_host(), &extra, welcome(1));
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    // The harness's host has no `df`, so a warning about the disk comes first; the last line
    // is the agent's reason for ending.
    assert_eq!(
        ran.err.lines().last(),
        Some(
            "js-agent: --mount-provider-user tierstore: no user \"_js-no-such-user\" in the \
             directory service"
        ),
        "{}",
        ran.err
    );
    assert_eq!(hello, Value::Null);

    for (flag, value) in [
        ("--mount-provider", provider.as_str()),
        ("--mount-provider-user", "tierstore=nobody"),
        ("--mount-provider-wait", "60"),
        ("--native-trees-root", "/private/var/tierstore/fs"),
    ] {
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

/// Asserts through the whole macOS agent that `--native-trees-root` is refused before the
/// hello, with status 2 and the reason naming the flag by exact equality, when no
/// `--mount-provider` is given and when it names a directory that is no `--mount-root-ro`;
/// and that a trees root that is a `--mount-root-ro`, beside a provider, starts the agent,
/// whose hello reports the provider. Catches a Mac agent that takes a trees root whose reads
/// it would open to every run's trees, and one that refuses the shape the proof Mac runs.
#[test]
fn a_mac_agent_takes_a_trees_root_only_as_a_read_only_root_beside_a_provider() {
    let dir = Scratch::new("native-agent-trees-root");
    let base = native_flags(&dir);
    let trees = private(&dir, "trees");
    fs::create_dir(dir.path("provider")).unwrap();
    let provider = format!(
        "tierstore={}",
        dir.path("provider").join("provider.sock").display()
    );
    let run = |extra: &[&str]| {
        let mut given = base.clone();
        given.extend(extra.iter().map(|flag| (*flag).to_owned()));
        hello_on(&mac_host(), &given, welcome(1))
    };
    let canonical = fs::canonicalize(&trees).unwrap();
    let refused = [
        (
            vec!["--native-trees-root", trees.as_str()],
            "--native-trees-root: needs --mount-provider: the trees root is where a provider \
             serves its trees"
                .to_owned(),
        ),
        (
            vec!["--native-trees-root", &trees, "--mount-provider", &provider],
            format!(
                "--native-trees-root: {} is no --mount-root-ro",
                canonical.display()
            ),
        ),
    ];
    for (extra, why) in refused {
        let (ran, hello) = run(&extra);
        assert_eq!(ran.status(), code(2), "{}", ran.err);
        assert_eq!(
            ran.err.lines().last(),
            Some(format!("js-agent: {why}").as_str()),
            "{}",
            ran.err
        );
        assert_eq!(hello, Value::Null);
    }
    let root = format!("trees={trees}");
    let (ran, hello) = run(&[
        "--native-trees-root",
        &trees,
        "--mount-provider",
        &provider,
        "--mount-root-ro",
        &root,
    ]);
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    assert_eq!(hello["capabilities"]["mount-provider.tierstore"], "true");
}

/// Asserts that an agent given `--native-accounts` and no `--native-account-range`, one given a
/// range the pool refuses, and one given more ready accounts than slots end with status 2
/// before the hello, naming the flag at fault. It catches an agent that registers as a native
/// host with a pool it cannot open, which would refuse every job.
#[test]
fn an_agent_with_a_pool_it_cannot_make_ends_before_the_hello() {
    let dir = Scratch::new("native-agent-range");
    let extra = native_flags(&dir);
    let mut low = without(&extra, "--native-account-range");
    low.extend(["--native-account-range".to_owned(), "500-510".to_owned()]);
    let mut ready = extra.clone();
    ready.extend(["--native-account-ready".to_owned(), "11".to_owned()]);
    let cases = [
        (
            without(&extra, "--native-account-range"),
            "--native-accounts: needs --native-account-range",
        ),
        (low, "--native-account-range: "),
        (
            ready,
            "--native-account-ready: 11 is more than the 10 slots of --native-account-range",
        ),
    ];
    for (flags, said) in cases {
        let (ran, hello) = hello_on(&mac_host(), &flags, welcome(1));
        assert_eq!(ran.status(), code(2), "{said}: {}", ran.err);
        assert!(ran.err.contains(said), "{said}: {}", ran.err);
        assert_eq!(hello, Value::Null, "{said}: nothing was sent");
    }
}

/// Asserts what the job flags require of a native agent: a state root and a scratch root
/// (the taint marker and each run's logs are kept there) and a `--stop-grace` that is
/// accepted. It catches a native agent that starts with no place for its taint marker.
#[test]
fn a_native_agent_needs_its_roots() {
    let dir = Scratch::new("native-agent-roots");
    let accounts = dir.path("accounts").display().to_string();
    let (state, scratch) = (private(&dir, "state"), private(&dir, "scratch"));
    let base = [
        "--native-accounts",
        &accounts,
        "--native-account-range",
        ACCOUNT_RANGE,
    ];
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
        why.starts_with("--state-root: required with --native-accounts"),
        "{why}"
    );
    let why = Local::from_cli(&cli(&["--state-root", &state]), 0).unwrap_err();
    assert!(
        why.starts_with("--scratch-root: required with --native-accounts"),
        "{why}"
    );
    let both = ["--state-root", &state, "--scratch-root", &scratch];
    let local = Local::from_cli(&cli(&both), 0).unwrap();
    assert!(matches!(local.backend, Some(Backend::Native(_))));
    let graced = [&both[..], &["--stop-grace", "5"]].concat();
    let local = Local::from_cli(&cli(&graced), 0).unwrap();
    assert_eq!(local.stop_grace.as_secs(), 5);
}

/// Asserts that the system machine makes the native runtime for the native backend, with no
/// helper to ask and no warning: an agent that has not found Full Disk Access (`not-granted` or
/// `undetermined`) gets a runtime with no pool, which refuses every native start
/// `no-full-disk-access` and whose self-test has nothing to run. It catches a native agent that
/// falls back to an engine, one that opens the pool of an agent that cannot delete an account,
/// and one that fails to start for want of a grant it reports.
#[test]
fn the_system_machine_opens_the_native_runtime() {
    let platform = js_agent::Platform {
        os: "macos".to_owned(),
        arch: "aarch64".to_owned(),
    };
    for held in [FullDiskAccess::NotGranted, FullDiskAccess::Undetermined] {
        let backend = Backend::Native(settings(held));
        let build = RuntimeBuild {
            backend: &backend,
            host_id: "h1",
            platform: &platform,
            stop_grace: std::time::Duration::from_secs(1),
        };
        let (runtime, warnings) = System::new().runtime(&build).unwrap();
        assert_eq!(warnings, Vec::<String>::new(), "{held:?}");
        let refused = runtime
            .check_native(&super::native("/usr/bin/true", &[]))
            .unwrap_err();
        assert_eq!(refused.code, refusal::NO_FULL_DISK_ACCESS, "{held:?}");
    }
}

/// Asserts that a native agent that is not root is refused at start, naming the flag, and
/// that the agent's token, instance and CA files, the pool's directory (`--native-accounts`),
/// the places only the privacy controls guard and `--native-deny-read` are read-denied beside
/// its roots. It catches an agent that takes leases and fails every start at the first
/// `chown`, and a secret file or the pool's ledger that a run can read.
#[test]
fn a_native_agent_must_be_root_and_denies_its_secret_files() {
    let dir = Scratch::new("native-agent-euid");
    let accounts = dir.path("accounts").display().to_string();
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
        "--native-accounts",
        &accounts,
        "--native-account-range",
        ACCOUNT_RANGE,
        "--native-homes",
        "/Users",
        "--state-root",
        &state,
        "--scratch-root",
        &scratch,
    ];
    let cli = Cli::try_parse_from(argv).unwrap();
    let why = Local::from_cli(&cli, 501).unwrap_err();
    assert!(
        why.starts_with("--native-accounts: the agent must run as root"),
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
    let real_accounts = fs::canonicalize(dir.path("")).unwrap().join("accounts");
    assert!(
        denied.contains(&real_accounts.to_str().unwrap()),
        "the pool's directory is not denied: {denied:?}"
    );
    assert!(denied.contains(&"/private/etc/kcpassword"));
    for place in PRIVACY_ONLY_PLACES {
        assert!(denied.contains(&place), "{place} is not denied: {denied:?}");
    }
}

/// The flags of a Mac agent that runs native jobs and keeps a store of bundles.
fn store_flags(dir: &Scratch, extra: &[&str]) -> Vec<String> {
    let mut flags = native_flags(dir);
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
    let bundles = Scratch::new("native-agent-store-placer");
    let store = FakeHost::new();
    store.set(|s| {
        s.open = Ok(Some(placer(&bundles, no_registry())));
        s.held = Ok(vec![digest('a')]);
    });
    let machine = Arc::new(FakeMachine::new());
    let extra = store_flags(&dir, &[]);
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
    let store = FakeHost::new();
    let machine = Arc::new(FakeMachine::new());
    let mut flags = store_flags(&other, &[]);
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
    let bundles = Scratch::new("native-agent-badstore-placer");
    let store = FakeHost::new();
    store.set(|s| s.open = Err("cannot open it".to_owned()));
    let machine = Arc::new(FakeMachine::new());
    let extra = store_flags(&dir, &[]);
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
/// makes: a runtime made over settings with a store runs the image start that carries a `run`
/// (its image is the bundle it runs from), and one made without refuses it. It catches a
/// store that is opened and never used.
#[test]
fn the_system_machine_gives_the_store_to_the_native_runtime() {
    let bundles = Scratch::new("native-agent-system-store-placer");
    let platform = js_agent::Platform {
        os: "macos".to_owned(),
        arch: "aarch64".to_owned(),
    };
    let plain = settings(FullDiskAccess::NotGranted);
    let stored = plain
        .clone()
        .with_bundles(placer(&bundles, no_registry()))
        .unwrap();
    assert!(stored.bundles().is_some() && plain.bundles().is_none());
    let runs_images: Vec<bool> = [plain, stored]
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
            runtime.runs_image_runs()
        })
        .collect();
    assert_eq!(runs_images, [false, true]);
}

/// Asserts that an agent whose cap is `on` ends with status 2 before the hello when it is
/// given no pf uids (the flag named), and when the anchor is not enforced (here the
/// `pfctl` it is told is not there), naming `--native-network` and the failed call; and that
/// the pf flags are accepted by the command line of an agent that is told them. It catches
/// an agent that says hello with the network on and nothing limiting it.
#[test]
fn an_agent_whose_cap_is_on_does_not_start_while_the_anchor_is_not_enforced() {
    let dir = Scratch::new("native-agent-pf");
    let mut extra = native_flags(&dir);
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
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict>{body}</dict></plist>\n"
    )
}

/// Asserts that a Mac agent whose scratch root's parent is on its state root's file system (as
/// after a boot, which mounts no scratch volume) asks `diskutil` to mount the scratch volume on
/// that parent, not on the scratch root, before it opens either root or looks at the bundle
/// store, and that one whose mount fails ends before the hello naming `--scratch-root` and the
/// command. Catches an agent that never mounts the volume, which would refuse to start after
/// every reboot, and one that mounts it on the scratch root, which leaves no volume for the
/// store beside it.
#[test]
fn a_mac_agent_mounts_the_scratch_volume_before_it_opens_its_roots() {
    let dir = Scratch::new("native-agent-scratch-volume");
    let mut extra = native_flags(&dir);
    // A store beside the scratch root, which does not exist yet: it can be made and checked
    // only once the volume is mounted, so a mount that fails ends the start first.
    let bundles = dir.path("bundles").display().to_string();
    extra.extend(
        [
            "--bundle-store",
            &bundles,
            "--bundle-budget-gib",
            "10",
            "--bundle-trust",
            "digest",
        ]
        .map(str::to_owned),
    );
    // The volume is mounted on the directory above the scratch root.
    let scratch = dir.path("scratch");
    let scratch = scratch.parent().unwrap().display().to_string();
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
    assert_eq!(ran_commands.last(), Some(&mount), "{ran_commands:?}");
    let diskutil: Vec<&String> = ran_commands
        .iter()
        .filter(|ran| ran.starts_with("/usr/sbin/diskutil "))
        .collect();
    assert_eq!(
        diskutil,
        [data, list, info, mount.as_str()],
        "{ran_commands:?}"
    );
}
