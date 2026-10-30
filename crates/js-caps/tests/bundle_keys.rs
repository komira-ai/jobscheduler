//! The bundle capability keys: named, and detected so an operator cannot claim them.

use js_caps::{BUNDLE, BUNDLE_ROOT, DETECTED_KEYS};

/// Asserts that the bundle keys carry the names the agent writes and that both are in
/// `DETECTED_KEYS`, the list the agent refuses an operator's `--capability` against. It
/// catches a key left out of the list: a flag could then claim a bundle the host does
/// not hold, and a job would be placed on it.
#[test]
fn a_flag_cannot_claim_a_bundle() {
    assert_eq!(BUNDLE, "bundle");
    assert_eq!(BUNDLE_ROOT, "bundle-root");
    assert!(DETECTED_KEYS.contains(&BUNDLE));
    assert!(DETECTED_KEYS.contains(&BUNDLE_ROOT));
}
