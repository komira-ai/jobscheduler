//! The capability keys a job request can require of a host: `disk-gib`, `exec` and
//! `os_version`.

use js_caps::{
    Consumable, DETECTED_KEYS, DISK_GIB, EXEC, FUSE_JOBS, FUSE_URING, LAYERED_ROOTFS, NodeCaps,
    OS_VERSION, ReportError, Request, RequestError, Unmet, XCODE,
};

fn req(props: &[(&str, &str)]) -> Result<Request, RequestError> {
    Request::parse(props.iter().copied())
}

/// A node reporting `entries` beside an arch.
fn node(entries: &[(&str, &str)]) -> Result<NodeCaps, ReportError> {
    let arch = [("arch", "x86_64")];
    NodeCaps::from_report(arch.iter().chain(entries).copied())
}

/// Asserts that the keys are the ones the design names, and that an operator cannot set
/// either (both are detected).
#[test]
fn the_keys_are_named_and_detected() {
    assert_eq!((DISK_GIB, EXEC), ("disk-gib", "exec"));
    assert!(DETECTED_KEYS.contains(&DISK_GIB));
    assert!(DETECTED_KEYS.contains(&EXEC));
    assert_eq!(Consumable::DiskGib.to_string(), "disk-gib");
}

/// Asserts that a request naming disk and mode is parsed, and that it reads back as the
/// same pairs.
#[test]
fn disk_and_exec_parse_and_round_trip() {
    let pairs = [("disk-gib", "200"), ("exec", "sandbox-exec")];
    let request = req(&pairs).unwrap();
    let back = request.properties();
    let again: Vec<(&str, &str)> = back.iter().map(|(k, v)| (*k, v.as_str())).collect();
    assert_eq!(Request::parse(again.iter().copied()).unwrap(), request);
    assert_eq!(request.exact("exec"), Some("sandbox-exec"));
    assert!(again.contains(&("disk-gib", "200")));
}

/// Asserts that a disk that is not a whole number, a mode that is not named, and a key
/// given twice are refused, each naming the key.
#[test]
fn bad_values_and_repeats_are_refused() {
    for value in ["", "-1", "1.5", "ten"] {
        let bad = RequestError::BadValue {
            key: "disk-gib".into(),
            value: value.into(),
        };
        assert_eq!(req(&[("disk-gib", value)]), Err(bad));
    }
    for value in ["", "Container", "vm"] {
        let bad = RequestError::BadValue {
            key: "exec".into(),
            value: value.into(),
        };
        assert_eq!(req(&[("exec", value)]), Err(bad));
    }
    let twice = req(&[("disk-gib", "1"), ("disk-gib", "2")]);
    assert_eq!(twice, Err(RequestError::Repeated("disk-gib".into())));
    let twice = req(&[("exec", "container"), ("exec", "container")]);
    assert_eq!(twice, Err(RequestError::Repeated("exec".into())));
}

/// Asserts that disk is a minimum with the exact edge accepted, that a node reporting no
/// disk has none, and that the miss is named like any other.
#[test]
fn disk_is_a_minimum_and_a_miss_is_named() {
    let have = node(&[("disk-gib", "100")]).unwrap();
    assert!(req(&[("disk-gib", "99")]).unwrap().matches(&have));
    assert!(req(&[("disk-gib", "100")]).unwrap().matches(&have));
    let more = req(&[("disk-gib", "101")]).unwrap();
    assert_eq!(
        more.unmet(&have),
        [Unmet::Consumable {
            what: Consumable::DiskGib,
            want: 101,
            have: 100
        }]
    );
    assert_eq!(more.unmet(&have)[0].to_string(), "disk-gib>=101 (has 100)");
    let bare = node(&[]).unwrap();
    assert!(!req(&[("disk-gib", "1")]).unwrap().matches(&bare));
    assert!(req(&[("disk-gib", "0")]).unwrap().matches(&bare));
}

/// Asserts that a mode is matched against the one the node reports, and that a node
/// reporting none matches no request for a mode while a request for none matches all.
#[test]
fn exec_is_matched_against_the_reported_mode() {
    let container = node(&[("exec", "container")]).unwrap();
    let mac = node(&[("exec", "sandbox-exec")]).unwrap();
    let bare = node(&[]).unwrap();
    let sandbox = req(&[("exec", "sandbox-exec")]).unwrap();
    assert!(sandbox.matches(&mac));
    assert!(!sandbox.matches(&container));
    assert!(!sandbox.matches(&bare));
    assert_eq!(
        sandbox.unmet(&container),
        [Unmet::Exact {
            key: "exec",
            want: "sandbox-exec"
        }]
    );
    assert!(Request::default().matches(&container));
}

/// Asserts that a report whose disk is not a whole number is refused, as every countable
/// entry is.
#[test]
fn a_report_with_a_bad_disk_is_refused() {
    assert!(node(&[("disk-gib", "lots")]).is_err());
}

/// Asserts that the detected keys are exactly these sixteen, and that a request may name only
/// `disk-gib`, `exec`, `layered-rootfs`, `fuse-jobs`, `fuse-uring`, `os_version` and `xcode` of
/// them (the layered root because the server asks for it of a job with layers, mounts or host
/// ports, the FUSE keys of a job that needs the FUSE device, `os_version` of a job that asks
/// for a least macOS, `xcode` of a job that names a build): each of the other nine is refused
/// as an unknown key, naming it. It catches a detected key
/// added without the agent detecting it, and a host
/// property a request could ask for though no agent reports it, so that a job naming it
/// could be placed nowhere.
#[test]
fn a_request_names_only_some_detected_keys() {
    let detected = [
        "runtime",
        "gpu-toolkit",
        "gpu-passthrough",
        "exec",
        "disk-gib",
        "run-files",
        "scratch-quota",
        "scratch-default-gib",
        "bundle",
        "bundle-root",
        "layered-rootfs",
        "fuse-jobs",
        "fuse-uring",
        "os_version",
        "xcode",
        "full-disk-access",
    ];
    assert_eq!(DETECTED_KEYS.as_slice(), detected.as_slice());
    let nameable = [
        DISK_GIB,
        EXEC,
        LAYERED_ROOTFS,
        FUSE_JOBS,
        FUSE_URING,
        OS_VERSION,
        XCODE,
    ];
    for key in DETECTED_KEYS.iter().filter(|key| !nameable.contains(key)) {
        let refused = req(&[(key, "true")]);
        assert_eq!(refused, Err(RequestError::UnknownKey((*key).to_owned())));
    }
    let named = [
        (DISK_GIB, "1"),
        (EXEC, "container"),
        (LAYERED_ROOTFS, "true"),
        (FUSE_JOBS, "true"),
        (FUSE_URING, "true"),
        (OS_VERSION, "26"),
        (XCODE, "16C5032a"),
    ];
    assert!(req(&named).is_ok());
}

/// Asserts that `os_version` is a least version, by major: a node that reports 27 meets a
/// request for 26 and for 27 and not for 28, the miss named in full; a node that reports
/// `26.4` is the major 26, and a request for `26.6` is the major 26 too; a node that reports
/// none, or text that is no version, meets no request for one, and a request that names none
/// is met by every node, as is one for 0. Catches the key refused as unknown (a job asking
/// for a version would then be placed nowhere), a version compared as text (`27` would not
/// meet `26`), and a minor version compared (a node on 26.4 would not meet a request for
/// 26.6, though a hello reports the major alone and the job API takes a whole major only).
#[test]
fn os_version_is_a_least_major_version() {
    let on = |version: &str| node(&[(OS_VERSION, version)]).unwrap();
    let ask = |version: &str| req(&[(OS_VERSION, version)]).unwrap();
    assert_eq!(Consumable::OsVersion.to_string(), "os_version");
    assert!(ask("26").matches(&on("27")));
    assert!(ask("27").matches(&on("27")));
    let later = ask("28");
    let on_27 = on("27");
    assert_eq!(
        later.unmet(&on_27),
        [Unmet::Consumable {
            what: Consumable::OsVersion,
            want: 28,
            have: 27
        }]
    );
    assert_eq!(
        later.unmet(&on_27)[0].to_string(),
        "os_version>=28 (has 27)"
    );
    assert!(ask("26").matches(&on("26.4")));
    assert!(ask("26.6").matches(&on("26.4")));
    assert!(!ask("27").matches(&on("26.4")));
    for unread in [node(&[]).unwrap(), on("beta"), on("")] {
        assert!(!ask("26").matches(&unread));
        assert!(Request::default().matches(&unread));
        assert!(ask("0").matches(&unread));
    }
    let minor = ask("26.6");
    assert_eq!(minor.properties(), [(OS_VERSION, "26".to_owned())]);
}

/// Asserts that a request whose `os_version` is no version, or that names it twice, is
/// refused naming the key, and that a report that names it twice is refused, even when the
/// first value is no version. Catches a bad version read as 0, which every node meets (the
/// previous test asserts that a request for 0 is).
#[test]
fn a_bad_or_repeated_os_version_is_refused() {
    for value in ["", "beta", "-1", ".5"] {
        let bad = RequestError::BadValue {
            key: OS_VERSION.into(),
            value: value.into(),
        };
        assert_eq!(req(&[(OS_VERSION, value)]), Err(bad));
    }
    let twice = req(&[(OS_VERSION, "26"), (OS_VERSION, "27")]);
    assert_eq!(twice, Err(RequestError::Repeated(OS_VERSION.into())));
    for first in ["26", "beta"] {
        let twice = node(&[(OS_VERSION, first), (OS_VERSION, "27")]);
        assert_eq!(twice, Err(ReportError::Repeated(OS_VERSION.into())));
    }
}

/// Asserts that the retired tree capability key, which no agent of any release reported, is
/// an unknown key to a request since its removal: a request naming it is refused with
/// `UnknownKey` naming it, and the message says so in full. It catches the key left among
/// the exact keys, where a job naming it would parse and then be placed nowhere.
#[test]
fn the_retired_tree_key_is_unknown_to_a_request() {
    let retired = "tierstore-trees";
    let refused = req(&[(retired, "true")]);
    assert_eq!(refused, Err(RequestError::UnknownKey(retired.to_owned())));
    let said = refused.unwrap_err().to_string();
    assert_eq!(said, r#"unknown capability key "tierstore-trees""#);
}
