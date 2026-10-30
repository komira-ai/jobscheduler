//! The bundle flags, checked together: what is accepted, the defaults, and every refusal.
//! Each test says the defect it catches.

use std::path::PathBuf;

use clap::Parser;
use js_agent::bundle::{Pull, Settings, Trust};
use js_agent::{Cli, Platform};
use js_agent_wire::ReadModel;

use super::fake::{FakeHost, apfs, digest};
use crate::support::platform;

const STORE: &str = "/Volumes/work/bundles";
const SCRATCH: &str = "/Volumes/work/scratch";

fn cli(extra: &[&str]) -> Cli {
    let base = [
        "js-agent",
        "--server",
        "http://127.0.0.1:1",
        "--pool",
        "macs",
    ];
    Cli::try_parse_from(base.iter().chain(extra.iter())).unwrap()
}

fn mac() -> Platform {
    platform("macos", "aarch64")
}

/// A host whose allow list holds one digest.
fn host() -> std::sync::Arc<FakeHost> {
    let host = FakeHost::new();
    host.file(
        "/etc/js/allow",
        &format!("# the release\n{}\n", digest('a')),
    );
    host
}

/// The flags of a farm host: a store, a scratch root, a budget and an allow list.
fn farm() -> Vec<&'static str> {
    vec![
        "--bundle-store",
        STORE,
        "--scratch-root",
        SCRATCH,
        "--bundle-budget-gib",
        "40",
        "--bundle-allow-file",
        "/etc/js/allow",
    ]
}

fn with(extra: &[&'static str]) -> Vec<&'static str> {
    let mut all = farm();
    all.extend_from_slice(extra);
    all
}

fn checked(args: &[&str]) -> Result<Option<Settings>, String> {
    Settings::from_cli(&cli(args), &mac(), &*host())
}

fn refused(args: &[&str]) -> String {
    checked(args).unwrap_err()
}

/// A farm host's flags give the store, the budget in bytes and the defaults the
/// rules name: trust `list`, pull `never`, the open read model, 30 minutes of minimum age, no
/// low-water mark. It catches a budget kept in GiB, a default of `digest` (any submitter
/// could run any bundle) or a default of `missing`.
#[test]
fn a_farm_host_gets_the_safe_defaults() {
    let settings = checked(&farm()).unwrap().unwrap();
    assert_eq!(settings.store.to_str(), Some(STORE));
    assert_eq!(settings.scratch.to_str(), Some(SCRATCH));
    assert_eq!(settings.budget_bytes, 40 << 30);
    assert_eq!(settings.low_water_bytes, 0);
    assert_eq!(settings.pull, Pull::Never);
    assert_eq!(settings.trust, Trust::List);
    assert_eq!(settings.read_model, ReadModel::Open);
    assert_eq!(settings.min_age_min, 30);
    assert!(settings.registries.is_empty());
    assert_eq!(
        settings.allow_file.as_deref().and_then(|p| p.to_str()),
        Some("/etc/js/allow")
    );
    assert!(settings.deny_file.is_none());
}

/// Every value flag reaches the settings: the low-water mark in bytes, the minimum age, the
/// three optional list files, a registry of each kind (the private one marked), pull
/// `missing`. It catches a flag that is parsed and dropped.
#[test]
fn every_flag_reaches_the_settings() {
    let host = host();
    for (path, text) in [
        ("/etc/js/deny", digest('b')),
        ("/etc/js/keep", digest('a')),
        ("/etc/js/prefetch", String::new()),
    ] {
        host.file(path, &text);
    }
    let args = with(&[
        "--bundle-low-water-gib",
        "5",
        "--bundle-min-age-min",
        "7",
        "--bundle-deny-file",
        "/etc/js/deny",
        "--bundle-keep-file",
        "/etc/js/keep",
        "--bundle-prefetch-file",
        "/etc/js/prefetch",
        "--bundle-pull",
        "missing",
        "--bundle-registry-allow",
        "ghcr.io/komira-ai/",
        "--bundle-registry-allow-private",
        "reg.internal:5000/js",
    ]);
    let settings = Settings::from_cli(&cli(&args), &mac(), &*host)
        .unwrap()
        .unwrap();
    assert_eq!(settings.low_water_bytes, 5 << 30);
    assert_eq!(settings.min_age_min, 7);
    assert_eq!(settings.pull, Pull::Missing);
    let files = [
        &settings.deny_file,
        &settings.keep_file,
        &settings.prefetch_file,
    ];
    let names: Vec<_> = files
        .iter()
        .map(|f| f.as_deref().and_then(|p| p.to_str()))
        .collect();
    assert_eq!(
        names,
        [
            Some("/etc/js/deny"),
            Some("/etc/js/keep"),
            Some("/etc/js/prefetch")
        ]
    );
    let registries: Vec<_> = settings
        .registries
        .iter()
        .map(|r| (r.host.as_str(), r.prefix.as_str(), r.private))
        .collect();
    assert_eq!(
        registries,
        [
            ("ghcr.io", "komira-ai", false),
            ("reg.internal:5000", "js", true)
        ]
    );
}

/// No bundle flag means no bundle store, on any host. It catches a Linux or container host
/// that is made to carry bundle settings it never asked for.
#[test]
fn without_a_store_flag_there_are_no_settings() {
    assert_eq!(checked(&[]), Ok(None));
    let linux = platform("linux", "x86_64");
    assert_eq!(Settings::from_cli(&cli(&[]), &linux, &*host()), Ok(None));
}

/// Each bundle flag given without `--bundle-store` ends the start naming that flag. It
/// catches a flag that silently does nothing, which an operator would take for a policy
/// that is in force.
#[test]
fn a_bundle_flag_without_a_store_names_itself() {
    let cases: [&[&str]; 12] = [
        &["--bundle-budget-gib", "1"],
        &["--bundle-low-water-gib", "1"],
        &["--bundle-pull", "never"],
        &["--bundle-registry-allow", "a.io/x"],
        &["--bundle-registry-allow-private", "a.io/x"],
        &["--bundle-min-age-min", "1"],
        &["--bundle-keep-file", "/k"],
        &["--bundle-deny-file", "/d"],
        &["--bundle-prefetch-file", "/p"],
        &["--bundle-trust", "list"],
        &["--bundle-allow-file", "/a"],
        &["--read-model", "open"],
    ];
    for args in cases {
        let why = refused(args);
        assert_eq!(
            why,
            format!("{}: needs --bundle-store", args[0]),
            "{args:?}"
        );
    }
}

/// The store itself: a Mac only, an absolute path, a scratch root to share a volume with, a
/// budget above zero, and a path short enough to be the `bundle-root` capability. It catches
/// a store on Linux (where its checks cannot run), a relative path that moves with the
/// working directory, and a hello the server would refuse.
#[test]
fn the_store_needs_a_mac_a_path_a_scratch_root_and_a_budget() {
    let linux = platform("linux", "x86_64");
    let why = Settings::from_cli(&cli(&farm()), &linux, &*host()).unwrap_err();
    assert_eq!(why, "--bundle-store: bundles are for macOS hosts");
    let why = refused(&["--bundle-store", "relative/store"]);
    assert_eq!(why, "--bundle-store: must be an absolute path");
    let why = refused(&["--bundle-store", STORE]);
    assert!(
        why.starts_with("--bundle-store: needs --scratch-root"),
        "{why}"
    );
    let no_budget = ["--bundle-store", STORE, "--scratch-root", SCRATCH];
    assert!(refused(&no_budget).starts_with("--bundle-budget-gib: required"));
    let zero = [&no_budget[..], &["--bundle-budget-gib", "0"]].concat();
    assert!(refused(&zero).starts_with("--bundle-budget-gib: required"));
    let long = format!("/{}", "a".repeat(128));
    let args = [
        "--bundle-store",
        long.as_str(),
        "--scratch-root",
        SCRATCH,
        "--bundle-budget-gib",
        "1",
        "--bundle-allow-file",
        "/etc/js/allow",
    ];
    let why = refused(&args);
    assert_eq!(
        why,
        "--bundle-store: the path is too long to report as bundle-root"
    );
}

/// The trust rules: `list` is the default and needs an allow file that holds a digest;
/// `digest` takes none; `digest` with `--bundle-pull missing` is refused (any submitter
/// could make the host download and unpack); `missing` needs a registry. It catches the
/// farm default being open, a list host with nothing on its list, and the combination that
/// turns a job's digest into a download.
#[test]
fn the_trust_rules_refuse_what_is_unsafe() {
    let base = [
        "--bundle-store",
        STORE,
        "--scratch-root",
        SCRATCH,
        "--bundle-budget-gib",
        "1",
    ];
    let why = refused(&base);
    assert_eq!(why, "--bundle-trust list: needs --bundle-allow-file");
    let host = FakeHost::new();
    host.file("/empty", "# nothing yet\n\n");
    let args = [&base[..], &["--bundle-allow-file", "/empty"]].concat();
    let why = Settings::from_cli(&cli(&args), &mac(), &*host).unwrap_err();
    assert_eq!(why, "--bundle-allow-file: lists no digest");
    let args = [&base[..], &["--bundle-trust", "digest"]].concat();
    let developer = checked(&args).unwrap().unwrap();
    assert_eq!(
        (developer.trust, developer.allow_file),
        (Trust::Digest, None)
    );
    let args = [&args[..], &["--bundle-allow-file", "/etc/js/allow"]].concat();
    assert_eq!(
        refused(&args),
        "--bundle-allow-file: needs --bundle-trust list"
    );
    let args = [
        &base[..],
        &[
            "--bundle-trust",
            "digest",
            "--bundle-pull",
            "missing",
            "--bundle-registry-allow",
            "ghcr.io/komira-ai/",
        ],
    ]
    .concat();
    let why = refused(&args);
    assert!(
        why.starts_with("--bundle-pull missing: not with --bundle-trust digest"),
        "{why}"
    );
    let args = with(&["--bundle-pull", "missing"]);
    let why = refused(&args);
    assert!(
        why.starts_with("--bundle-pull missing: needs --bundle-registry-allow"),
        "{why}"
    );
}

/// A list file that cannot be read, or has a line that is not a digest, ends the start
/// naming the flag; a malformed line is named by its number and not echoed. It catches an
/// allow list or deny list that is silently empty, and a digest text pushed into a log.
#[test]
fn a_list_file_that_cannot_be_read_stops_the_start() {
    let why = refused(&with(&["--bundle-deny-file", "/etc/js/missing"]));
    assert!(
        why.starts_with("--bundle-deny-file: /etc/js/missing:"),
        "{why}"
    );
    let host = FakeHost::new();
    host.file("/bad", &format!("{}\nnot-a-digest-secret\n", digest('a')));
    let flags = |flag: &'static str| {
        let mut args = farm();
        args.retain(|a| *a != "--bundle-allow-file" && *a != "/etc/js/allow");
        args.extend([flag, "/bad"]);
        args.extend(["--bundle-trust", "digest"]);
        args
    };
    for flag in [
        "--bundle-deny-file",
        "--bundle-keep-file",
        "--bundle-prefetch-file",
    ] {
        let why = Settings::from_cli(&cli(&flags(flag)), &mac(), &*host).unwrap_err();
        assert_eq!(why, format!("{flag}: line 2 is not a sha256 digest"));
    }
    let args = with(&[]);
    let why = Settings::from_cli(&cli(&args), &mac(), &*FakeHost::new()).unwrap_err();
    assert!(
        why.starts_with("--bundle-allow-file: /etc/js/allow:"),
        "{why}"
    );
}

/// A choice flag with a value that is none of its choices names the flag and the choices,
/// and `--read-model allow` is refused as not available yet. It catches a typo taken for a
/// default and an allow-list profile that is promised and not there.
#[test]
fn choices_are_checked_and_the_allow_read_model_is_not_available() {
    for (flag, value, names) in [
        ("--bundle-pull", "always", "never, missing"),
        ("--bundle-trust", "any", "list, digest"),
        ("--read-model", "closed", "open, allow"),
    ] {
        let why = refused(&with(&[flag, value]));
        assert_eq!(why, format!("{flag}: \"{value}\" is none of {names}"));
    }
    let why = refused(&with(&["--read-model", "allow"]));
    assert!(
        why.starts_with("--read-model: allow is not available yet"),
        "{why}"
    );
    let open = checked(&with(&["--read-model", "open"])).unwrap().unwrap();
    assert_eq!(open.read_model, ReadModel::Open);
}

/// The start checks of the machine are asked of the store and the scratch root, and what
/// they refuse ends the start with the flag in front: a store and a scratch root on two
/// volumes, a volume that is not APFS, one that ignores ownership, one that is noexec, and a
/// kernel above securelevel 0. It catches a store whose sealing means nothing, and clones
/// that cannot work across volumes.
#[test]
fn the_volume_and_securelevel_checks_refuse_the_start() {
    let host = host();
    let args = farm();
    let go = |host: &FakeHost| Settings::from_cli(&cli(&args), &mac(), host);
    assert!(go(&host).unwrap().is_some());
    let asked = host.get(|s| s.started.clone());
    assert_eq!(asked, vec![(PathBuf::from(STORE), PathBuf::from(SCRATCH))]);
    host.set(|s| {
        s.volumes = vec![(STORE.into(), apfs(1)), (SCRATCH.into(), apfs(2))];
    });
    let why = go(&host).unwrap_err();
    assert!(why.starts_with("--bundle-store: "), "{why}");
    assert!(why.contains("different volumes"), "{why}");
    let mut hfs = apfs(1);
    hfs.fs_type = "hfs".to_owned();
    let mut owned = apfs(1);
    owned.flags = 0x0020_0000;
    let mut noexec = apfs(1);
    noexec.flags = 0x4;
    for (facts, said) in [
        (hfs, "not apfs"),
        (owned, "ignoring ownership"),
        (noexec, "noexec"),
    ] {
        host.set(|s| s.volumes = vec![("/".into(), facts)]);
        let why = go(&host).unwrap_err();
        assert!(why.contains(said), "{why}");
    }
    host.set(|s| {
        s.volumes = vec![("/".into(), apfs(1))];
        s.securelevel = 1;
    });
    assert!(go(&host).unwrap_err().contains("securelevel"));
}
