//! The order uids are tried in.

use std::ops::RangeInclusive;

/// Every uid of `range` once, starting after `after` (the uid handed out last) and wrapping,
/// so a uid is used again as late as the range allows: anything a proof could not see has
/// the longest time to show in a leak scan before the same uid runs again. From the first
/// uid when `after` is `None` or outside the range; nothing for an empty range.
pub(super) fn after(range: &RangeInclusive<u32>, after: Option<u32>) -> impl Iterator<Item = u32> {
    let first = *range.start();
    let len = (u64::from(*range.end()) + 1).saturating_sub(u64::from(first));
    let start = match after {
        Some(uid) if range.contains(&uid) => u64::from(uid - first) + 1,
        _ => 0,
    };
    (0..len).map(move |i| {
        let offset = (start + i) % len;
        first.saturating_add(u32::try_from(offset).unwrap_or(u32::MAX))
    })
}

#[cfg(test)]
mod tests;
