//! The capability key of a job's layered root, `layered-rootfs`: named, detected so an
//! operator cannot claim it, kept from a node's report, and matched exactly by a request.

use js_caps::{
    DETECTED_KEYS, FLAG_SET, LAYERED_ROOTFS, NodeCaps, ReportError, Request, RequestError, Unmet,
};

fn req(props: &[(&str, &str)]) -> Result<Request, RequestError> {
    Request::parse(props.iter().copied())
}

/// A node reporting `entries` beside an arch.
fn node(entries: &[(&str, &str)]) -> Result<NodeCaps, ReportError> {
    let arch = [("arch", "x86_64")];
    NodeCaps::from_report(arch.iter().chain(entries).copied())
}

/// Asserts that the key carries the name the agent writes and is in `DETECTED_KEYS`, the
/// list the agent refuses an operator's `--capability` against. Catches the key left out of
/// the list: a flag could then claim layered roots on a host whose agent ignores a start's
/// layers, and a job placed there would run on its image alone.
#[test]
fn a_flag_cannot_claim_layered_rootfs() {
    assert_eq!(LAYERED_ROOTFS, "layered-rootfs");
    assert_eq!(FLAG_SET, "true");
    assert!(DETECTED_KEYS.contains(&LAYERED_ROOTFS));
}

/// Asserts that a request naming the key parses and reads back as the same pair, and that a
/// request naming it twice is refused naming it. Catches the key missing from the exact
/// keys, where a request naming it is refused as an unknown key and no job with layers,
/// mounts or host ports can be placed at all.
#[test]
fn a_request_for_layered_rootfs_parses() {
    let request = req(&[(LAYERED_ROOTFS, FLAG_SET)]).unwrap();
    assert_eq!(request.exact(LAYERED_ROOTFS), Some("true"));
    assert_eq!(
        request.properties(),
        [("layered-rootfs", "true".to_owned())]
    );
    let twice = req(&[(LAYERED_ROOTFS, "true"), (LAYERED_ROOTFS, "true")]);
    assert_eq!(twice, Err(RequestError::Repeated("layered-rootfs".into())));
}

/// Asserts that a node's `layered-rootfs=true` survives `from_report` and satisfies the
/// request, while a node that reports no such key, or another value, does not, and the miss
/// is named. Catches a report reader that drops the key, which would leave every layered job
/// unplaceable, and a match that ignores it, which would send a layered job to an agent that
/// runs it on its image alone.
#[test]
fn a_reported_layered_rootfs_is_matched_exactly() {
    let layered = req(&[(LAYERED_ROOTFS, FLAG_SET)]).unwrap();
    let reporting = node(&[(LAYERED_ROOTFS, FLAG_SET)]).unwrap();
    assert!(layered.matches(&reporting));
    let unmet = [Unmet::Exact {
        key: "layered-rootfs",
        want: "true",
    }];
    let silent = node(&[]).unwrap();
    assert_eq!(layered.unmet(&silent), unmet);
    assert_eq!(layered.unmet(&silent)[0].to_string(), "layered-rootfs=true");
    let other = node(&[(LAYERED_ROOTFS, "false")]).unwrap();
    assert_eq!(layered.unmet(&other), unmet);
    assert!(Request::default().matches(&silent));
}

/// Asserts that a report naming the key twice is refused naming it, as every single-valued
/// entry is. Catches a report whose second entry silently replaces the first.
#[test]
fn a_report_with_layered_rootfs_twice_is_refused() {
    let twice = node(&[(LAYERED_ROOTFS, "true"), (LAYERED_ROOTFS, "true")]);
    assert_eq!(twice, Err(ReportError::Repeated("layered-rootfs".into())));
}
