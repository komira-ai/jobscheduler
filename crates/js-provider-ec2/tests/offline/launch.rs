//! The launch side's names, which are a contract with what is already tagged in an
//! account.

use js_provider_ec2::{ACQUIRE_TAG, LABEL_PREFIX, NAME_TAG, SCHEDULER_TAG};

/// Asserts that the tags that mark an instance are spelled as they are in accounts that
/// already hold instances this scheduler launched: renaming one would make those
/// instances look foreign.
#[test]
fn the_marking_tags_are_spelled_as_documented() {
    assert_eq!(SCHEDULER_TAG, "js-scheduler");
    assert_eq!(ACQUIRE_TAG, "js-acquire-id");
    assert_eq!(NAME_TAG, "Name");
    assert_eq!(LABEL_PREFIX, "js-label:");
}
