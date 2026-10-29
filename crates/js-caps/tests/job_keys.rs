//! The capability keys a job request can require of a host: `disk-gib` and `exec`.

use js_caps::{
    Consumable, DETECTED_KEYS, DISK_GIB, EXEC, NodeCaps, ReportError, Request, RequestError, Unmet,
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
