//! The GPU passthrough capability key: named, and detected so an operator cannot claim it.

use js_caps::{DETECTED_KEYS, FLAG_SET, GPU_PASSTHROUGH};

/// Asserts that the key carries the name the agent writes in its hello, with the flag value,
/// and is in `DETECTED_KEYS`, the list the agent refuses an operator's `--capability`
/// against. It catches a key left out of the list: a flag could then claim
/// `gpu-passthrough` for a host whose jobs are given no GPU.
#[test]
fn flag_cannot_claim_gpu_passthrough() {
    assert_eq!(GPU_PASSTHROUGH, "gpu-passthrough");
    assert_eq!(FLAG_SET, "true");
    assert!(DETECTED_KEYS.contains(&GPU_PASSTHROUGH));
}
