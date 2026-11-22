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
    assert_eq!(order(700..=704, Some(702)), [703, 704, 700, 701, 702]);
    assert_eq!(order(700..=704, Some(704)), [700, 701, 702, 703, 704]);
    assert_eq!(order(700..=704, Some(700)), [701, 702, 703, 704, 700]);
    assert_eq!(order(700..=704, None), [700, 701, 702, 703, 704]);
    assert_eq!(order(700..=704, Some(7)), [700, 701, 702, 703, 704]);
    assert_eq!(order(700..=704, Some(705)), [700, 701, 702, 703, 704]);
    assert_eq!(order(700..=700, Some(700)), [700]);
}

/// Asserts that an empty range has no uid to try, and that the last uid a range can hold
/// does not overflow.
#[test]
fn an_empty_range_has_no_uid_and_the_top_of_the_range_does_not_overflow() {
    // Ranges made from variables: written as literals they are a lint error.
    let (first, last) = (800, 799);
    assert!(order(first..=last, None).is_empty());
    let (first, last) = (5, 0);
    assert!(order(first..=last, Some(3)).is_empty());
    assert_eq!(
        order(u32::MAX - 1..=u32::MAX, Some(u32::MAX - 1)),
        [u32::MAX, u32::MAX - 1]
    );
    assert_eq!(order(u32::MAX..=u32::MAX, None), [u32::MAX]);
}
