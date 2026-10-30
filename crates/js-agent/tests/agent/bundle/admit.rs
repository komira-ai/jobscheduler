//! The checks of a start that names a bundle, over a fake host. Each test says the defect it
//! catches.

use std::sync::Arc;

use clap::Parser;
use js_agent::bundle::{Admission, Fetch, HostXcode, Needs, Settings};
use js_agent::{Cli, Platform};
use js_agent_wire::JobSpec;
use js_agent_wire::refusal::{self, RefusalClass, bundle_class};
use js_bundle::{DottedVersion, XcodeComponent, XcodeNeed};
use serde_json::json;

use super::fake::{FakeHost, digest};
use crate::support::platform;

const NAME: &str = "ghcr.io/komira-ai/js/toolchain:1";

fn version(text: &str) -> DottedVersion {
    DottedVersion::parse(text).unwrap()
}

fn mac() -> Platform {
    platform("macos", "aarch64")
}

/// A native start that pins `pin`.
fn start(pin: &str) -> JobSpec {
    serde_json::from_value(json!({
        "run_id": "r1",
        "lease_id": "l1",
        "native": { "program": "@bundle/clang" },
        "limits": { "cpu_millis": 1000, "memory_bytes": 1 << 30, "wall_ms": 60000 },
        "bundle": pin
    }))
    .unwrap()
}

fn pinned() -> JobSpec {
    start(&format!("{NAME}@{}", digest('a')))
}

/// A host that holds the bundle `a`, which needs macOS 14.0 and no Xcode, on macOS 14.8,
/// with an allow list that names it.
fn host() -> Arc<FakeHost> {
    let host = FakeHost::new();
    host.file("/etc/js/allow", &format!("{}\n", digest('a')));
    host.set(|s| {
        s.held = Ok(vec![digest('a')]);
        s.needs.insert(
            digest('a'),
            Needs {
                os_version: version("14.0"),
                xcode: None,
            },
        );
    });
    host
}

fn settings_of(host: &FakeHost, extra: &[&str]) -> Settings {
    let mut args = vec![
        "js-agent",
        "--server",
        "http://127.0.0.1:1",
        "--pool",
        "macs",
        "--bundle-store",
        "/Volumes/work/bundles",
        "--scratch-root",
        "/Volumes/work/scratch",
        "--bundle-budget-gib",
        "10",
        "--bundle-low-water-gib",
        "20",
    ];
    args.extend_from_slice(extra);
    let cli = Cli::try_parse_from(args).unwrap();
    Settings::from_cli(&cli, &mac(), host).unwrap().unwrap()
}

/// The admission of a host that trusts its allow list.
fn listed(host: &Arc<FakeHost>, extra: &[&str]) -> Admission {
    let mut args = vec!["--bundle-allow-file", "/etc/js/allow"];
    args.extend_from_slice(extra);
    Admission::new(settings_of(host, &args), host.clone())
}

/// The code a start is refused with; panics when it is admitted. Every code has a class.
fn refused(admission: &Admission, spec: &JobSpec) -> &'static str {
    let (code, why) = admission.admit(spec).unwrap_err();
    assert!(bundle_class(code).is_some(), "{code} has no class: {why}");
    code
}

/// A start with no bundle is none of this module's business, and a start whose bundle is
/// held, allowed, in an allowed namespace and runnable here is admitted. It catches a gate
/// that refuses what nothing is wrong with.
#[test]
fn a_start_without_a_bundle_and_a_good_bundle_pass() {
    let host = host();
    let admission = listed(&host, &[]);
    let mut plain = pinned();
    plain.bundle = None;
    assert_eq!(admission.admit(&plain), Ok(()));
    assert_eq!(admission.admit(&pinned()), Ok(()));
}

/// A pin that is not a pinned bundle (which the wire has refused already) is refused
/// `invalid-bundle`, a request fault, and not a panic. It catches a gate that trusts its
/// caller.
#[test]
fn a_pin_that_does_not_parse_is_the_requests_fault() {
    let host = host();
    let admission = listed(&host, &[]);
    let code = refused(&admission, &start("toolchain:latest"));
    assert_eq!(code, refusal::INVALID_BUNDLE);
    assert_eq!(bundle_class(code), Some(RefusalClass::Request));
}

/// A digest on the deny list is refused `bundle-denied` even when the allow list names it;
/// a deny file that cannot be read, or is malformed, refuses every bundle (the list that
/// cannot be read could be the one that names it); and a change to the file is seen by the
/// next start. It catches a revocation that needs a restart and a broken list that lets
/// everything through.
#[test]
fn a_denied_digest_is_refused_at_once_and_a_broken_deny_list_refuses_all() {
    let host = host();
    host.file("/etc/js/deny", &format!("{}\n", digest('b')));
    let admission = listed(&host, &["--bundle-deny-file", "/etc/js/deny"]);
    assert_eq!(admission.admit(&pinned()), Ok(()));
    host.file("/etc/js/deny", &format!("# revoked\n{}\n", digest('a')));
    assert_eq!(refused(&admission, &pinned()), refusal::BUNDLE_DENIED);
    host.file("/etc/js/deny", "garbage\n");
    assert_eq!(refused(&admission, &pinned()), refusal::BUNDLE_DENIED);
    host.set(|s| {
        s.files.remove(std::path::Path::new("/etc/js/deny"));
    });
    assert_eq!(refused(&admission, &pinned()), refusal::BUNDLE_DENIED);
}

/// With trust `list` a digest that is not on the allow list is refused
/// `bundle-not-allowed`, and so is every digest when the list cannot be read or is
/// malformed; a change to the list is seen by the next start; with trust `digest` the list
/// is not consulted. It catches a farm host that runs what a job names.
#[test]
fn a_digest_off_the_allow_list_is_refused_and_a_broken_list_refuses_all() {
    let host = host();
    let admission = listed(&host, &[]);
    host.file("/etc/js/allow", &format!("{}\n", digest('b')));
    assert_eq!(refused(&admission, &pinned()), refusal::BUNDLE_NOT_ALLOWED);
    host.file(
        "/etc/js/allow",
        &format!("{}\n{}\n", digest('b'), digest('a')),
    );
    assert_eq!(admission.admit(&pinned()), Ok(()));
    host.file("/etc/js/allow", "garbage\n");
    assert_eq!(refused(&admission, &pinned()), refusal::BUNDLE_NOT_ALLOWED);
    host.set(|s| {
        s.files.clear();
    });
    assert_eq!(refused(&admission, &pinned()), refusal::BUNDLE_NOT_ALLOWED);
    let developer = Admission::new(
        settings_of(&host, &["--bundle-trust", "digest"]),
        host.clone(),
    );
    assert_eq!(developer.admit(&pinned()), Ok(()));
}

/// When a registry is allowed, a name outside it is refused `bundle-registry-not-allowed`:
/// another namespace, a half-matching namespace, another host, a name with no host. A name inside it, with or
/// without a tag, passes. It catches a job that chooses the registry the host reaches.
#[test]
fn a_name_outside_the_allowed_registries_is_refused() {
    let host = host();
    let admission = listed(&host, &["--bundle-registry-allow", "ghcr.io/komira-ai/"]);
    let at = |name: &str| start(&format!("{name}@{}", digest('a')));
    for name in [NAME, "ghcr.io/komira-ai/js/toolchain"] {
        assert_eq!(admission.admit(&at(name)), Ok(()), "{name}");
    }
    for name in [
        "ghcr.io/other/toolchain",
        "ghcr.io/komira-ai-evil/toolchain",
        "docker.io/komira-ai/toolchain",
        "toolchain",
    ] {
        assert_eq!(
            refused(&admission, &at(name)),
            refusal::BUNDLE_REGISTRY_NOT_ALLOWED,
            "{name}"
        );
    }
    let private = listed(
        &host,
        &["--bundle-registry-allow-private", "reg.lan:5000/js"],
    );
    assert_eq!(private.admit(&at("reg.lan:5000/js/toolchain")), Ok(()));
    assert_eq!(
        refused(&private, &at("ghcr.io/komira-ai/toolchain")),
        refusal::BUNDLE_REGISTRY_NOT_ALLOWED
    );
}

/// A bundle the store does not hold, with `--bundle-pull never`, is refused
/// `bundle-not-held` (the farm's fault: it was not prefetched), and so is any bundle when the
/// store cannot be listed. It catches a job placed on a host that would have to fetch, and a
/// store read failure taken for "held".
#[test]
fn a_bundle_the_store_lacks_is_refused_when_nothing_fetches() {
    let host = host();
    let admission = listed(&host, &[]);
    host.set(|s| s.held = Ok(vec![digest('b')]));
    let code = refused(&admission, &pinned());
    assert_eq!(code, refusal::BUNDLE_NOT_HELD);
    assert_eq!(bundle_class(code), Some(RefusalClass::Farm));
    assert!(host.get(|s| s.fetches.is_empty()));
    host.set(|s| s.held = Err("Permission denied".to_owned()));
    assert_eq!(refused(&admission, &pinned()), refusal::BUNDLE_NOT_HELD);
}

/// With `--bundle-pull missing` a fetch begins for a bundle the store lacks and the start is
/// refused `bundle-pending` (retriable); the fetch is asked once with the pin's name and
/// digest. A digest that names an index is `bundle-index` (the request's fault) and a fetch
/// that cannot run or did not verify is `bundle-fetch-failed`. It catches a worker thread
/// that waits for a download, and an index taken for a manifest.
#[test]
fn a_missing_bundle_starts_a_fetch_and_the_start_is_retriable() {
    let host = host();
    let missing = &[
        "--bundle-pull",
        "missing",
        "--bundle-registry-allow",
        "ghcr.io/komira-ai/",
    ];
    let admission = listed(&host, missing);
    host.set(|s| s.held = Ok(Vec::new()));
    assert_eq!(refused(&admission, &pinned()), refusal::BUNDLE_PENDING);
    let asked = host.get(|s| s.fetches.clone());
    assert_eq!(asked, vec![(NAME.to_owned(), digest('a'))]);
    host.set(|s| s.fetch = Fetch::Index);
    let code = refused(&admission, &pinned());
    assert_eq!(code, refusal::BUNDLE_INDEX);
    assert_eq!(bundle_class(code), Some(RefusalClass::Request));
    host.set(|s| s.fetch = Fetch::Failed("digest mismatch".to_owned()));
    let (code, why) = admission.admit(&pinned()).unwrap_err();
    assert_eq!(
        (code, why.as_str()),
        (refusal::BUNDLE_FETCH_FAILED, "digest mismatch")
    );
}

/// A fetch is not begun when the store is at its budget or the volume is under its
/// low-water mark (10 GiB budget, 20 GiB low water here): the start is refused
/// `bundle-no-space`, and so is a start when either number cannot be read. It catches a fetch
/// into a full store and an unreadable volume taken for an empty one.
#[test]
fn no_fetch_begins_without_room() {
    let host = host();
    let missing = &[
        "--bundle-pull",
        "missing",
        "--bundle-registry-allow",
        "ghcr.io/komira-ai/",
    ];
    let admission = listed(&host, missing);
    host.set(|s| s.held = Ok(Vec::new()));
    let cases: [(Result<u64, String>, Result<u64, String>); 5] = [
        (Ok(10 << 30), Ok(100 << 30)),
        (Ok(11 << 30), Ok(100 << 30)),
        (Ok(0), Ok((20 << 30) - 1)),
        (Err("io".to_owned()), Ok(100 << 30)),
        (Ok(0), Err("io".to_owned())),
    ];
    for (used, free) in cases {
        host.set(|s| {
            s.used = used.clone();
            s.free = free.clone();
        });
        let code = refused(&admission, &pinned());
        assert_eq!(code, refusal::BUNDLE_NO_SPACE, "{used:?} {free:?}");
        assert_eq!(bundle_class(code), Some(RefusalClass::Farm));
    }
    assert!(host.get(|s| s.fetches.is_empty()));
    host.set(|s| {
        s.used = Ok((10 << 30) - 1);
        s.free = Ok(20 << 30);
    });
    assert_eq!(refused(&admission, &pinned()), refusal::BUNDLE_PENDING);
}

/// A held bundle whose config cannot be read is refused `bundle-not-held`: the store is
/// damaged, and the farm's fault. It catches a bundle run without its constraints.
#[test]
fn a_held_bundle_with_no_readable_config_is_refused() {
    let host = host();
    let admission = listed(&host, &[]);
    host.set(|s| s.needs.clear());
    assert_eq!(refused(&admission, &pinned()), refusal::BUNDLE_NOT_HELD);
}

/// The macOS floor: a host below the bundle's `os.version` is refused `bundle-os-too-old`;
/// equal and above pass (14.8 against 14.8, and 14.10 against 14.9, which a text compare
/// gets wrong); a host whose version cannot be read is refused the same. It catches a
/// bundle built for a newer macOS run on an older one.
#[test]
fn a_bundle_for_a_newer_macos_is_refused() {
    let host = host();
    let admission = listed(&host, &[]);
    let needing = |floor: &str| {
        host.set(|s| {
            s.needs.insert(
                digest('a'),
                Needs {
                    os_version: version(floor),
                    xcode: None,
                },
            );
        });
    };
    needing("15.0");
    assert_eq!(refused(&admission, &pinned()), refusal::BUNDLE_OS_TOO_OLD);
    needing("14.8");
    assert_eq!(admission.admit(&pinned()), Ok(()));
    host.set(|s| s.os = Ok(version("14.10")));
    needing("14.9");
    assert_eq!(admission.admit(&pinned()), Ok(()));
    needing("14.11");
    assert_eq!(refused(&admission, &pinned()), refusal::BUNDLE_OS_TOO_OLD);
    host.set(|s| s.os = Err("sw_vers: not found".to_owned()));
    assert_eq!(refused(&admission, &pinned()), refusal::BUNDLE_OS_TOO_OLD);
}

/// The Xcode need: a bundle that calls into Xcode is refused `bundle-xcode-mismatch` on a
/// host with none, one too old, or one without a component it names; it passes when one Xcode
/// meets both the version and the components (not a version from one and components from
/// another). It catches a toolchain started on a host whose Xcode it would fail against.
#[test]
fn an_xcode_need_is_met_by_one_xcode() {
    let host = host();
    let admission = listed(&host, &[]);
    let need = XcodeNeed {
        min: version("16.0"),
        components: vec![XcodeComponent::Clang, XcodeComponent::Swift],
    };
    host.set(|s| {
        s.needs.get_mut(&digest('a')).unwrap().xcode = Some(need.clone());
    });
    assert_eq!(
        refused(&admission, &pinned()),
        refusal::BUNDLE_XCODE_MISMATCH
    );
    let xcode = |build: &str, v: &str, components: &[XcodeComponent]| HostXcode {
        build: build.to_owned(),
        version: version(v),
        components: components.to_vec(),
    };
    let both = [XcodeComponent::Clang, XcodeComponent::Swift];
    host.set(|s| s.xcodes = vec![xcode("15F31d", "15.4", &both)]);
    assert_eq!(
        refused(&admission, &pinned()),
        refusal::BUNDLE_XCODE_MISMATCH
    );
    host.set(|s| s.xcodes = vec![xcode("16C5032a", "16.2", &[XcodeComponent::Clang])]);
    assert_eq!(
        refused(&admission, &pinned()),
        refusal::BUNDLE_XCODE_MISMATCH
    );
    host.set(|s| {
        s.xcodes = vec![
            xcode("15F31d", "15.4", &both),
            xcode("16C5032a", "16.2", &[XcodeComponent::Clang]),
        ];
    });
    assert_eq!(
        refused(&admission, &pinned()),
        refusal::BUNDLE_XCODE_MISMATCH
    );
    host.set(|s| s.xcodes.push(xcode("16E140", "16.4", &both)));
    assert_eq!(admission.admit(&pinned()), Ok(()));
}

/// The checks run in a fixed order: with everything wrong, each fix reveals the next
/// refusal: denied, not allowed, registry, not held, macOS, Xcode. It catches a reorder that
/// turns a revocation into a "not held" the farm would retry.
#[test]
fn the_checks_run_in_order() {
    let host = host();
    host.file("/etc/js/deny", "");
    let admission = listed(
        &host,
        &[
            "--bundle-deny-file",
            "/etc/js/deny",
            "--bundle-registry-allow",
            "ghcr.io/komira-ai/",
        ],
    );
    let outside = start(&format!("docker.io/x/y@{}", digest('a')));
    host.file("/etc/js/deny", &format!("{}\n", digest('a')));
    host.file("/etc/js/allow", &format!("{}\n", digest('b')));
    host.set(|s| {
        s.held = Ok(Vec::new());
        s.os = Ok(version("13.0"));
        s.needs.get_mut(&digest('a')).unwrap().xcode = Some(XcodeNeed {
            min: version("99.0"),
            components: Vec::new(),
        });
    });
    assert_eq!(refused(&admission, &outside), refusal::BUNDLE_DENIED);
    host.file("/etc/js/deny", "");
    assert_eq!(refused(&admission, &outside), refusal::BUNDLE_NOT_ALLOWED);
    host.file("/etc/js/allow", &format!("{}\n", digest('a')));
    assert_eq!(
        refused(&admission, &outside),
        refusal::BUNDLE_REGISTRY_NOT_ALLOWED
    );
    assert_eq!(refused(&admission, &pinned()), refusal::BUNDLE_NOT_HELD);
    host.set(|s| s.held = Ok(vec![digest('a')]));
    assert_eq!(refused(&admission, &pinned()), refusal::BUNDLE_OS_TOO_OLD);
    host.set(|s| s.os = Ok(version("14.8")));
    assert_eq!(
        refused(&admission, &pinned()),
        refusal::BUNDLE_XCODE_MISMATCH
    );
}

/// The admission prints its settings and not its host, so a log line or a failed assertion
/// that formats the lifecycle's configuration shows the policy. It catches a debug output
/// that dumps a host's state.
#[test]
fn the_admission_prints_its_settings() {
    let host = host();
    let admission = listed(&host, &[]);
    let shown = format!("{admission:?}");
    assert!(
        shown.starts_with("Admission { settings: Settings {"),
        "{shown}"
    );
    assert!(shown.contains("budget_bytes: 10737418240"), "{shown}");
    assert!(!shown.contains("FakeHost"), "{shown}");
}
