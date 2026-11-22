//! Tests of the order uids are tried in.

use super::*;

fn order(range: RangeInclusive<u32>, last: Option<u32>) -> Vec<u32> {
    after(&range, last).collect()
}

/// Asserts that the order starts after the uid given last and wraps round the range once, so
/// a uid comes back as late as the range allows; and starts at the first uid when none was
/// given or the last is not in the range (a range that was moved).
#[test]
fn the_order_starts_after_the_last_uid_and_wraps() {
    assert_eq!(order(600..=604, Some(602)), [603, 604, 600, 601, 602]);
    assert_eq!(order(600..=604, Some(604)), [600, 601, 602, 603, 604]);
    assert_eq!(order(600..=604, Some(600)), [601, 602, 603, 604, 600]);
    assert_eq!(order(600..=604, None), [600, 601, 602, 603, 604]);
    assert_eq!(order(600..=604, Some(7)), [600, 601, 602, 603, 604]);
    assert_eq!(order(600..=604, Some(605)), [600, 601, 602, 603, 604]);
    assert_eq!(order(600..=600, Some(600)), [600]);
}

/// Asserts that an empty range has no uid to try, and that the last uid a range can hold
/// does not overflow.
#[test]
fn an_empty_range_has_no_uid_and_the_top_of_the_range_does_not_overflow() {
    // Ranges made from variables: written as literals they are a lint error.
    let (first, last) = (700, 699);
    assert!(order(first..=last, None).is_empty());
    let (first, last) = (5, 0);
    assert!(order(first..=last, Some(3)).is_empty());
    assert_eq!(
        order(u32::MAX - 1..=u32::MAX, Some(u32::MAX - 1)),
        [u32::MAX, u32::MAX - 1]
    );
    assert_eq!(order(u32::MAX..=u32::MAX, None), [u32::MAX]);
}
