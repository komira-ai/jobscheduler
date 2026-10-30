//! A bundle flag that cannot work stops the whole agent before it registers: exit status 2,
//! the flag named on stderr and nothing sent. Each test says the defect it catches.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use super::fake::{FakeHost, apfs};
use crate::jobs::fake::FakeMachine;
use crate::support::{
    CountingSleeper, Host, Jobs, Lowest, Scratch, Script, ScriptConnector, code, flags, linux_host,
    mac_host, run_with_store, welcome,
};

const STORE: &str = "/Volumes/work/bundles";

/// Runs the agent on `host` with `extra` flags and a private scratch root; returns stderr
/// after checking that it ended with status 2 and sent nothing.
fn refused(name: &str, extra: &[&str], host: &Host, store: Arc<FakeHost>) -> String {
    let dir = Scratch::new(name);
    let scratch = dir.path("scratch");
    fs::create_dir(&scratch).unwrap();
    fs::set_permissions(&scratch, fs::Permissions::from_mode(0o700)).unwrap();
    let mut owned = vec!["--scratch-root".to_owned(), scratch.display().to_string()];
    owned.extend(extra.iter().map(|flag| (*flag).to_owned()));
    let all: Vec<&str> = owned.iter().map(String::as_str).collect();
    let (script, sent) = Script::new(vec![welcome(1)]);
    let connector = ScriptConnector::of(script);
    let mut sleeper = CountingSleeper::stopping_at(1);
    let jobs: Jobs<'_> = (Arc::new(FakeMachine::new()), None);
    let ran = run_with_store(
        &flags(&all),
        host,
        &connector,
        &mut sleeper,
        &mut Lowest,
        jobs,
        store,
    );
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    assert!(sent.borrow().is_empty(), "something was sent");
    ran.err
}

/// Trust `list` with no allow file, and with an allow file that lists nothing, stop the agent.
/// It catches a farm host that starts with no list and then refuses (or, worse, admits)
/// everything.
#[test]
fn list_mode_with_no_usable_file_does_not_start() {
    let store = ["--bundle-store", STORE, "--bundle-budget-gib", "40"];
    let err = refused("start-list", &store, &mac_host(), FakeHost::new());
    assert!(
        err.contains("--bundle-trust list: needs --bundle-allow-file"),
        "{err}"
    );
    let host = FakeHost::new();
    host.file("/etc/js/allow", "# empty\n");
    let all = [&store[..], &["--bundle-allow-file", "/etc/js/allow"]].concat();
    let err = refused("start-empty", &all, &mac_host(), host);
    assert!(
        err.contains("--bundle-allow-file: lists no digest"),
        "{err}"
    );
}

/// Trust `digest` with `--bundle-pull missing` stops the agent. It catches the combination
/// that lets any submitter make root download and unpack.
#[test]
fn digest_trust_with_missing_pull_does_not_start() {
    let all = [
        "--bundle-store",
        STORE,
        "--bundle-budget-gib",
        "40",
        "--bundle-trust",
        "digest",
        "--bundle-pull",
        "missing",
        "--bundle-registry-allow",
        "ghcr.io/komira-ai/",
    ];
    let err = refused("start-digest-missing", &all, &mac_host(), FakeHost::new());
    assert!(
        err.contains("--bundle-pull missing: not with --bundle-trust digest"),
        "{err}"
    );
}

/// A store and a scratch root on different volumes stop the agent, whichever way the fake
/// volumes are laid out; so does a kernel above securelevel 0. It catches clones that cannot
/// work across volumes and a `disk-gib` read from the wrong one.
#[test]
fn a_store_off_the_scratch_volume_does_not_start() {
    let all = [
        "--bundle-store",
        STORE,
        "--bundle-budget-gib",
        "40",
        "--bundle-trust",
        "digest",
    ];
    let store = FakeHost::new();
    store.set(|s| s.volumes = vec![(STORE.into(), apfs(1)), ("/".into(), apfs(2))]);
    let err = refused("start-volumes", &all, &mac_host(), store);
    assert!(
        err.contains("--bundle-store: the store and the scratch root are on different volumes"),
        "{err}"
    );
    let store = FakeHost::new();
    store.set(|s| s.securelevel = 2);
    let err = refused("start-securelevel", &all, &mac_host(), store);
    assert!(
        err.contains("--bundle-store: kern.securelevel is 2"),
        "{err}"
    );
}

/// A bundle flag on a host that is not a Mac stops the agent, and a bundle flag without a
/// store names itself. It catches a Linux host that carries a policy it cannot enforce.
#[test]
fn bundle_flags_need_a_mac_and_a_store() {
    let on_linux = [
        "--bundle-store",
        STORE,
        "--bundle-budget-gib",
        "40",
        "--bundle-trust",
        "digest",
    ];
    let err = refused("start-linux", &on_linux, &linux_host(), FakeHost::new());
    assert!(
        err.contains("--bundle-store: bundles are for macOS hosts"),
        "{err}"
    );
    let err = refused(
        "start-nostore",
        &["--bundle-pull", "never"],
        &mac_host(),
        FakeHost::new(),
    );
    assert!(err.contains("--bundle-pull: needs --bundle-store"), "{err}");
}
