//! The capability keys of the FUSE test environment, `fuse-jobs` and `fuse-uring`: named,
//! detected so an operator cannot claim them, kept from a node's report, matched exactly by
//! a request, and passed through from a platform's properties.

use js_caps::{
    DETECTED_KEYS, FLAG_SET, FUSE_JOBS, FUSE_URING, NodeCaps, ReportError, Request, RequestError,
    Unmet,
};

fn req(props: &[(&str, &str)]) -> Result<Request, RequestError> {
    Request::parse(props.iter().copied())
}

/// A node reporting `entries` beside an arch.
fn node(entries: &[(&str, &str)]) -> Result<NodeCaps, ReportError> {
    let arch = [("arch", "x86_64")];
    NodeCaps::from_report(arch.iter().chain(entries).copied())
}

/// Asserts that both keys carry the names the agent writes and are in `DETECTED_KEYS`, the
/// list the agent refuses an operator's `--capability` against. Catches a key left out of the
/// list: a flag could then claim the FUSE device on a host whose agent gives no job one.
#[test]
fn a_flag_cannot_claim_the_fuse_keys() {
    assert_eq!(FUSE_JOBS, "fuse-jobs");
    assert_eq!(FUSE_URING, "fuse-uring");
    assert_eq!(FLAG_SET, "true");
    assert!(DETECTED_KEYS.contains(&FUSE_JOBS));
    assert!(DETECTED_KEYS.contains(&FUSE_URING));
}

/// Asserts that a request naming `fuse-jobs` parses and reads back as the same pair, and that
/// naming it twice is refused naming it. Catches the key missing from the exact keys, where a
/// job that needs FUSE is refused as an unknown key and can never be placed.
#[test]
fn a_request_for_fuse_jobs_parses() {
    let request = req(&[(FUSE_JOBS, FLAG_SET)]).unwrap();
    assert_eq!(request.exact(FUSE_JOBS), Some("true"));
    assert_eq!(request.properties(), [("fuse-jobs", "true".to_owned())]);
    let twice = req(&[(FUSE_JOBS, "true"), (FUSE_JOBS, "true")]);
    assert_eq!(twice, Err(RequestError::Repeated("fuse-jobs".into())));
}

/// Asserts the same of `fuse-uring`, and that a request naming both keeps both.
#[test]
fn a_request_for_fuse_uring_parses() {
    let request = req(&[(FUSE_URING, FLAG_SET)]).unwrap();
    assert_eq!(request.exact(FUSE_URING), Some("true"));
    assert_eq!(request.properties(), [("fuse-uring", "true".to_owned())]);
    let twice = req(&[(FUSE_URING, "true"), (FUSE_URING, "true")]);
    assert_eq!(twice, Err(RequestError::Repeated("fuse-uring".into())));
    let both = req(&[(FUSE_URING, "true"), (FUSE_JOBS, "true")]).unwrap();
    assert_eq!(
        both.properties(),
        [
            ("fuse-jobs", "true".to_owned()),
            ("fuse-uring", "true".to_owned())
        ]
    );
}

/// Asserts that a node's `fuse-jobs=true` satisfies the request, and a node that reports no
/// such key, or another value, does not, and the miss is named. Catches a match that ignores
/// the key, which would send a FUSE job to an agent that gives it no device.
#[test]
fn a_reported_fuse_jobs_is_matched_exactly() {
    let wanted = req(&[(FUSE_JOBS, FLAG_SET)]).unwrap();
    let reporting = node(&[(FUSE_JOBS, FLAG_SET)]).unwrap();
    assert!(wanted.matches(&reporting));
    let unmet = [Unmet::Exact {
        key: "fuse-jobs",
        want: "true",
    }];
    let silent = node(&[]).unwrap();
    assert_eq!(wanted.unmet(&silent), unmet);
    assert_eq!(wanted.unmet(&silent)[0].to_string(), "fuse-jobs=true");
    let other = node(&[(FUSE_JOBS, "false")]).unwrap();
    assert_eq!(wanted.unmet(&other), unmet);
    assert!(Request::default().matches(&silent));
}

/// Asserts that a node's `fuse-uring=true` satisfies the request, and a node that reports
/// only `fuse-jobs`, or another value, does not, and the miss is named. Catches a match that
/// reads `fuse-uring` as `fuse-jobs`.
#[test]
fn a_reported_fuse_uring_is_matched_exactly() {
    let wanted = req(&[(FUSE_URING, FLAG_SET)]).unwrap();
    let reporting = node(&[(FUSE_JOBS, FLAG_SET), (FUSE_URING, FLAG_SET)]).unwrap();
    assert!(wanted.matches(&reporting));
    let unmet = [Unmet::Exact {
        key: "fuse-uring",
        want: "true",
    }];
    let without = node(&[(FUSE_JOBS, FLAG_SET)]).unwrap();
    assert_eq!(wanted.unmet(&without), unmet);
    assert_eq!(wanted.unmet(&without)[0].to_string(), "fuse-uring=true");
    let other = node(&[(FUSE_JOBS, FLAG_SET), (FUSE_URING, "false")]).unwrap();
    assert_eq!(wanted.unmet(&other), unmet);
}

/// Asserts that a report keeps both keys, and that a report naming either twice is refused
/// naming it, as every single-valued entry is. Catches a report reader that drops a key and a
/// report whose second entry silently replaces the first.
#[test]
fn a_report_keeps_both_fuse_keys_once_each() {
    let both = node(&[(FUSE_JOBS, FLAG_SET), (FUSE_URING, FLAG_SET)]).unwrap();
    let wants = req(&[(FUSE_JOBS, FLAG_SET), (FUSE_URING, FLAG_SET)]).unwrap();
    assert!(wants.matches(&both));
    let jobs_twice = node(&[(FUSE_JOBS, "true"), (FUSE_JOBS, "true")]);
    assert_eq!(jobs_twice, Err(ReportError::Repeated("fuse-jobs".into())));
    let uring_twice = node(&[(FUSE_URING, "true"), (FUSE_URING, "true")]);
    assert_eq!(uring_twice, Err(ReportError::Repeated("fuse-uring".into())));
}

/// Asserts that the platform properties `fuse-jobs` and `fuse-uring`, in any case of the
/// name, reach the request as the lower-case keys. Catches the keys unknown to
/// `from_platform`, where an action's platform asking for them would be ignored and the
/// action run on a host without the device.
#[test]
fn the_fuse_properties_pass_through_from_platform() {
    let lower = Request::from_platform([("fuse-jobs", "true"), ("fuse-uring", "true")]).unwrap();
    assert_eq!(
        lower.properties(),
        [
            ("fuse-jobs", "true".to_owned()),
            ("fuse-uring", "true".to_owned())
        ]
    );
    let upper = Request::from_platform([("FUSE-JOBS", "true"), ("Fuse-Uring", "true")]).unwrap();
    assert_eq!(upper, lower);
}

/// Asserts that a misspelt key is refused as an unknown key naming it. Catches a prefix match
/// that would read `fuse-job` as one of the keys.
#[test]
fn a_misspelt_fuse_key_is_unknown() {
    let refused = req(&[("fuse-job", "true")]);
    assert_eq!(
        refused,
        Err(RequestError::UnknownKey("fuse-job".to_owned()))
    );
}
