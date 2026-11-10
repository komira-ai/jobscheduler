//! The capability key of tree inputs, `tierstore-trees`: named, detected so an operator
//! cannot claim it, kept from a node's report, and matched exactly by a request.

use js_caps::{
    DETECTED_KEYS, FLAG_SET, NodeCaps, ReportError, Request, RequestError, TIERSTORE_TREES, Unmet,
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
/// the list: a flag could then claim tree inputs on a host with no `tierstore fs`, and a
/// tree job placed there would never start.
#[test]
fn a_flag_cannot_claim_tierstore_trees() {
    assert_eq!(TIERSTORE_TREES, "tierstore-trees");
    assert_eq!(FLAG_SET, "true");
    assert!(DETECTED_KEYS.contains(&TIERSTORE_TREES));
}

/// Asserts that a request naming the key parses and reads back as the same pair, and that a
/// request naming it twice is refused naming it. Catches the key missing from the exact
/// keys, where a request naming it is refused as an unknown key and no tree job can be
/// placed at all.
#[test]
fn a_request_for_tierstore_trees_parses() {
    let request = req(&[(TIERSTORE_TREES, FLAG_SET)]).unwrap();
    assert_eq!(request.exact(TIERSTORE_TREES), Some("true"));
    assert_eq!(
        request.properties(),
        [("tierstore-trees", "true".to_owned())]
    );
    let twice = req(&[(TIERSTORE_TREES, "true"), (TIERSTORE_TREES, "true")]);
    assert_eq!(twice, Err(RequestError::Repeated("tierstore-trees".into())));
}

/// Asserts that a node's `tierstore-trees=true` survives `from_report` and satisfies the
/// request, while a node that reports no such key, or another value, does not, and the miss
/// is named. Catches a report reader that drops the key, which would leave every tree job
/// unplaceable, and a match that ignores it, which would send a tree job to a host that
/// cannot attach its tree.
#[test]
fn a_reported_tierstore_trees_is_matched_exactly() {
    let trees = req(&[(TIERSTORE_TREES, FLAG_SET)]).unwrap();
    let reporting = node(&[(TIERSTORE_TREES, FLAG_SET)]).unwrap();
    assert!(trees.matches(&reporting));
    let unmet = [Unmet::Exact {
        key: "tierstore-trees",
        want: "true",
    }];
    let silent = node(&[]).unwrap();
    assert_eq!(trees.unmet(&silent), unmet);
    assert_eq!(trees.unmet(&silent)[0].to_string(), "tierstore-trees=true");
    let other = node(&[(TIERSTORE_TREES, "false")]).unwrap();
    assert_eq!(trees.unmet(&other), unmet);
    assert!(Request::default().matches(&silent));
}

/// Asserts that a report naming the key twice is refused naming it, as every single-valued
/// entry is. Catches a report whose second entry silently replaces the first.
#[test]
fn a_report_with_tierstore_trees_twice_is_refused() {
    let twice = node(&[(TIERSTORE_TREES, "true"), (TIERSTORE_TREES, "true")]);
    assert_eq!(twice, Err(ReportError::Repeated("tierstore-trees".into())));
}
