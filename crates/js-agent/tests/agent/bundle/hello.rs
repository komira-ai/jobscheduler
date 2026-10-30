//! What the agent says about its bundle store: the capabilities of the hello, the frame for a
//! local consumer, and the disk it offers beside the store. Each test says the defect it
//! catches.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use clap::Parser;
use js_agent::bundle::{HostXcode, Settings, hello_capabilities, report, reserve_bytes};
use js_agent::{Cli, Platform};
use js_agent_wire::limits::MAX_BUNDLE_CAPABILITY_DIGESTS;
use js_agent_wire::{ReadModel, limits};
use js_bundle::{DottedVersion, XcodeComponent};
use serde_json::Value;

use super::fake::{FakeHost, digest};
use crate::jobs::fake::FakeMachine;
use crate::support::{
    CountingSleeper, Lowest, Ran, Scratch, Script, ScriptConnector, code, flags, mac_host,
    platform, run_with_store, welcome,
};

const STORE: &str = "/Volumes/work/bundles";
const GIB: u64 = 1 << 30;

fn mac() -> Platform {
    platform("macos", "aarch64")
}

/// Runs a Mac agent with `extra` flags beside a scratch root, until its first wait; returns
/// what it printed and the hello it sent (`null` when it sent none). `free_gib` is what `df`
/// says the scratch volume has free.
fn run_hello(name: &str, extra: &[&str], store: Arc<FakeHost>, free_gib: u64) -> (Ran, Value) {
    let dir = Scratch::new(name);
    let scratch = dir.path("scratch");
    fs::create_dir(&scratch).unwrap();
    fs::set_permissions(&scratch, fs::Permissions::from_mode(0o700)).unwrap();
    let mut host = mac_host();
    let row = format!(
        "Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/disk3 9 9 {} 11% /\n",
        free_gib * GIB / 1024
    );
    host.commands = host
        .commands
        .with(&format!("/bin/df -P -k {}", scratch.display()), Ok(&row));
    let scratch_flag = scratch.display().to_string();
    let mut owned = vec!["--scratch-root".to_owned(), scratch_flag];
    owned.extend(extra.iter().map(|flag| (*flag).to_owned()));
    let all: Vec<&str> = owned.iter().map(String::as_str).collect();
    let (script, sent) = Script::new(vec![welcome(1)]);
    let connector = ScriptConnector::of(script);
    let mut sleeper = CountingSleeper::stopping_at(1);
    let jobs: crate::support::Jobs<'_> = (Arc::new(FakeMachine::new()), None);
    let ran = run_with_store(
        &flags(&all),
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

/// The flags of a developer host: a store with a budget of `budget` GiB, any digest.
fn store_flags(budget: &str) -> Vec<&str> {
    vec![
        "--bundle-store",
        STORE,
        "--bundle-budget-gib",
        budget,
        "--bundle-trust",
        "digest",
    ]
}

fn offered(hello: &Value) -> u64 {
    hello["shape"]["disk_bytes"].as_u64().unwrap_or(0)
}

/// The hello of a store that holds two bundles carries the store's root and the two
/// digests, ascending, comma-separated. It catches a capability a placement cannot read and
/// a root that is not the path the flag gave.
#[test]
fn the_hello_reports_the_root_and_the_held_digests() {
    let store = FakeHost::new();
    store.set(|s| s.held = Ok(vec![digest('a'), digest('b')]));
    let (ran, hello) = run_hello("hello-held", &store_flags("40"), store, 100);
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    let caps = &hello["capabilities"];
    assert_eq!(caps["bundle-root"], STORE);
    assert_eq!(caps["bundle"], format!("{},{}", digest('a'), digest('b')));
}

/// A store that holds nothing reports its root and no `bundle` key; an agent with no store
/// reports neither key. It catches a host that claims a toolchain it does not hold, and one
/// that claims a store it does not have.
#[test]
fn an_empty_store_reports_only_its_root_and_no_store_reports_nothing() {
    let (ran, hello) = run_hello("hello-empty", &store_flags("40"), FakeHost::new(), 100);
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    assert_eq!(hello["capabilities"]["bundle-root"], STORE);
    assert!(hello["capabilities"].get("bundle").is_none());
    let (ran, hello) = run_hello("hello-none", &[], FakeHost::new(), 100);
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    assert!(hello["capabilities"].get("bundle-root").is_none());
    assert!(hello["capabilities"].get("bundle").is_none());
}

/// A store that holds more bundles than the wire takes in one capability reports the first
/// sixteen and says so on stderr; the hello is still valid. It catches a hello the server
/// refuses, which would stop the agent for a store that grew.
#[test]
fn more_digests_than_the_wire_takes_report_the_first_and_warn() {
    let all: Vec<String> = (0..20).map(|n| format!("sha256:{n:064x}")).collect();
    let store = FakeHost::new();
    store.set(|s| s.held = Ok(all.clone()));
    let (ran, hello) = run_hello("hello-many", &store_flags("40"), store, 100);
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    let shown = hello["capabilities"]["bundle"].as_str().unwrap();
    assert_eq!(shown, all[..MAX_BUNDLE_CAPABILITY_DIGESTS].join(","));
    assert!(
        ran.err
            .contains("only the first 16 are reported in the hello"),
        "{}",
        ran.err
    );
}

/// A store that cannot be listed reports no bundle and warns with the reason. It catches a
/// listing error taken for a store with every bundle in it.
#[test]
fn a_store_that_cannot_be_listed_reports_no_bundle() {
    let store = FakeHost::new();
    store.set(|s| s.held = Err("Permission denied".to_owned()));
    let (ran, hello) = run_hello("hello-unlisted", &store_flags("40"), store, 100);
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    assert!(hello["capabilities"].get("bundle").is_none());
    assert!(
        ran.err
            .contains("cannot list the bundles (Permission denied)"),
        "{}",
        ran.err
    );
}

/// An operator cannot give `--capability bundle=...` or `bundle-root=...`: the keys are
/// detected. It catches a flag that claims a toolchain the store does not hold.
#[test]
fn an_operator_cannot_claim_a_bundle() {
    for key in ["bundle", "bundle-root"] {
        let claim = format!("{key}=x");
        let (ran, hello) = run_hello(
            "hello-claim",
            &["--capability", &claim],
            FakeHost::new(),
            100,
        );
        assert_eq!(ran.status(), code(2), "{}", ran.err);
        assert!(
            ran.err
                .contains(&format!("{key} is detected and cannot be set")),
            "{}",
            ran.err
        );
        assert_eq!(hello, Value::Null);
    }
}

/// The agent and the server write the capability keys the same way. It catches a key the
/// agent spells one way and the wire's limits know another.
#[test]
fn the_capability_keys_agree_between_the_crates() {
    assert_eq!(js_caps::BUNDLE, limits::BUNDLE_CAPABILITY);
    assert_eq!(js_caps::BUNDLE_ROOT, "bundle-root");
}

/// The disk offered is `floor((free - max(0, budget - used) - low_water) / GiB)`, in the
/// shape's `disk_bytes` and in `disk-gib` alike: the part of the budget the store has not
/// used is kept back from jobs. Rows: an empty store keeps the whole budget back; a used
/// store the rest; a store at or over its budget keeps nothing; a low-water mark comes off
/// after; a budget above the free space leaves 0; a store whose use cannot be read keeps its
/// whole budget back and warns. It catches a job promised space the store will take.
#[test]
fn the_offered_disk_leaves_out_the_unused_budget() {
    // (free GiB, budget GiB, used GiB or unreadable, extra flags, offered GiB)
    type Row = (
        u64,
        &'static str,
        Result<u64, ()>,
        &'static [&'static str],
        u64,
    );
    let rows: [Row; 8] = [
        (100, "40", Ok(10), &[], 70),
        (100, "40", Ok(0), &[], 60),
        (100, "40", Ok(40), &[], 100),
        (100, "40", Ok(50), &[], 100),
        (100, "40", Ok(10), &["--scratch-low-water-gib", "5"], 65),
        (30, "40", Ok(10), &[], 0),
        (100, "40", Err(()), &[], 60),
        (100, "1", Ok(0), &[], 99),
    ];
    for (n, (free, budget, used, extra, want)) in rows.into_iter().enumerate() {
        let store = FakeHost::new();
        store.set(|s| {
            s.used = used
                .map(|gib| gib * GIB)
                .map_err(|()| "io error".to_owned());
        });
        let mut all = store_flags(budget);
        all.extend_from_slice(extra);
        let (ran, hello) = run_hello(&format!("disk-{n}"), &all, store, free);
        assert_eq!(ran.status(), code(0), "{}", ran.err);
        assert_eq!(offered(&hello), want * GIB, "row {n}");
        assert_eq!(
            hello["capabilities"]["disk-gib"],
            want.to_string(),
            "row {n}"
        );
        let warned = ran
            .err
            .contains("cannot read the bytes the store uses (io error)");
        assert_eq!(warned, used.is_err(), "row {n}: {}", ran.err);
    }
}

/// Without a store the disk offered is the free space, as before. It catches a reserve taken
/// from a host that has no store.
#[test]
fn without_a_store_the_whole_free_space_is_offered() {
    let (ran, hello) = run_hello("disk-nostore", &[], FakeHost::new(), 100);
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    assert_eq!(offered(&hello), 100 * GIB);
    assert_eq!(hello["capabilities"]["disk-gib"], "100");
}

fn settings(host: &FakeHost) -> Settings {
    let mut args = vec![
        "js-agent",
        "--server",
        "http://127.0.0.1:1",
        "--pool",
        "macs",
        "--scratch-root",
        "/Volumes/work/scratch",
    ];
    args.extend(store_flags("10"));
    let cli = Cli::try_parse_from(args).unwrap();
    Settings::from_cli(&cli, &mac(), host).unwrap().unwrap()
}

/// The frame for a local consumer carries the same facts as the hello: the digests as the
/// store lists them, the root, the open read model and the Xcode builds the host found, each
/// once and in order; and it is a valid frame. It catches a consumer told something other
/// than the scheduler.
#[test]
fn the_caps_frame_carries_the_facts_of_the_hello() {
    let host = FakeHost::new();
    let xcode = |build: &str| HostXcode {
        build: build.to_owned(),
        version: DottedVersion::parse("16.0").unwrap(),
        components: vec![XcodeComponent::Clang],
    };
    host.set(|s| {
        s.held = Ok(vec![digest('a'), digest('b')]);
        s.xcodes = vec![xcode("16C5032a"), xcode("15F31d"), xcode("16C5032a")];
    });
    let reported = report(&settings(&host), &*host);
    assert!(reported.warnings.is_empty());
    let caps = reported.caps;
    assert_eq!(caps.validate(), Ok(()));
    assert_eq!(caps.bundles, [digest('a'), digest('b')]);
    assert_eq!(caps.bundle_root, STORE);
    assert_eq!(caps.read_model, ReadModel::Open);
    assert_eq!(caps.xcode, ["15F31d", "16C5032a"]);
    let hello = hello_capabilities(&caps);
    assert_eq!(
        hello,
        [
            (
                "bundle".to_owned(),
                format!("{},{}", digest('a'), digest('b'))
            ),
            ("bundle-root".to_owned(), STORE.to_owned()),
        ]
    );
}

/// The reserve is the budget less what the store uses, never below zero, and the whole budget
/// when the use cannot be read. It catches a wrapped subtraction.
#[test]
fn the_reserve_never_goes_below_zero() {
    let host = FakeHost::new();
    let settings = settings(&host);
    for (used, want) in [
        (0, 10 * GIB),
        (4 * GIB, 6 * GIB),
        (10 * GIB, 0),
        (u64::MAX, 0),
    ] {
        host.set(|s| s.used = Ok(used));
        assert_eq!(reserve_bytes(&settings, &*host), (want, None), "{used}");
    }
    host.set(|s| s.used = Err("io".to_owned()));
    let (reserve, warning) = reserve_bytes(&settings, &*host);
    assert_eq!(reserve, 10 * GIB);
    assert!(warning.unwrap().contains("(io)"));
}
