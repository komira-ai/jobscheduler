//! The disk-quota capability keys: named, and detected so an operator cannot claim them.

use js_caps::{DETECTED_KEYS, SCRATCH_DEFAULT_GIB, SCRATCH_QUOTA, SCRATCH_QUOTA_PROJECT};

/// Asserts that the quota keys carry the names the agent and the server both write, and
/// that both are in `DETECTED_KEYS`, the list the agent refuses an operator's
/// `--capability` against. It catches a key left out of the list: a flag could then
/// claim `scratch-quota` and a pool that names it would admit a host that enforces no
/// quota, whose jobs run with no disk bound while the server has booked them one.
#[test]
fn flag_cannot_claim_scratch_quota() {
    assert_eq!(SCRATCH_QUOTA, "scratch-quota");
    assert_eq!(SCRATCH_DEFAULT_GIB, "scratch-default-gib");
    assert_eq!(SCRATCH_QUOTA_PROJECT, "project");
    assert!(DETECTED_KEYS.contains(&SCRATCH_QUOTA));
    assert!(DETECTED_KEYS.contains(&SCRATCH_DEFAULT_GIB));
}

/// Asserts that no key is listed twice, so a list that grows by a copy-paste of an
/// existing key is caught.
#[test]
fn detected_keys_are_distinct() {
    let mut keys = DETECTED_KEYS.to_vec();
    keys.sort_unstable();
    keys.dedup();
    assert_eq!(keys.len(), DETECTED_KEYS.len());
}
