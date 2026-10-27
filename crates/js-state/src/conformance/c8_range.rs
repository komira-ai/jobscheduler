//! C8's range checks: the limit and lower bound, the reverse and the upper-bound scans, the
//! scan across concurrent commits, and the empty prefix. [`super::c8_bounded_range_scan`]
//! seeds the keys and runs them in turn.

use std::num::NonZeroUsize;
use std::ops::Bound;

use super::{C8_KEYS, assert_pages, c8_scan, keys_of, page, run, value};
use crate::store::Store;
use crate::types::{Cond, Direction, ResumeToken, ScanRange, Txn, Versioned};

/// C8 forward: the limit, the lower bound, the prefix and the token.
pub(super) fn c8_forward<S: Store>(store: &S) {
    let first = page(store, c8_scan(2));
    assert_eq!(
        keys_of(&first),
        ["c8/", "c8/a"],
        "C8 limit: a scan with limit 2 must return the first 2 keys of the prefix in key order"
    );
    assert_eq!(
        first.resume.as_ref().map(ResumeToken::key),
        Some("c8/a"),
        "C8 resume: a full page with keys after it names its last key"
    );
    assert_eq!(
        first.rows[1].1,
        value(store, "c8/a").expect("c8/a exists"),
        "C8 limit: a row carries the key's value, version and expiry"
    );
    let from = |lower, limit| ScanRange {
        lower,
        ..c8_scan(limit)
    };
    assert_eq!(
        keys_of(&page(store, from(Bound::Included("c8/c"), 2))),
        ["c8/c", "c8/d"],
        "C8 lower: an included lower key is the first row"
    );
    assert_eq!(
        keys_of(&page(store, from(Bound::Included("c8/bz"), 1))),
        ["c8/c"],
        "C8 lower: a lower key that is not stored begins at the next key"
    );
    // `c8/bb` expired between `c8/b` and `c8/c`: the page is still full, and since a
    // live key follows it, it still names its token.
    let around = page(store, from(Bound::Included("c8/b"), 2));
    assert_eq!(
        (
            keys_of(&around),
            around.resume.as_ref().map(ResumeToken::key)
        ),
        (vec!["c8/b", "c8/c"], Some("c8/c")),
        "C8 expiry: an expired key neither appears in a page, nor shortens it, nor costs it its token"
    );
    // The prefix: a lower bound past it finds nothing. `c8~` is stored past the prefix;
    // nothing is stored at or after `c9`, so that scan runs off the end of the store.
    for beyond in ["c8~", "c9"] {
        let past = page(store, from(Bound::Included(beyond), 4));
        assert_eq!(
            keys_of(&past),
            Vec::<&str>::new(),
            "C8 prefix: a lower bound past the prefix ({beyond}) gives an empty page"
        );
        assert_eq!(
            past.resume, None,
            "C8 prefix: a lower bound past the prefix ({beyond}) gives a final page"
        );
    }
    let whole = page(store, c8_scan(C8_KEYS.len()));
    let listed: Vec<(String, Versioned)> = store.scan_prefix("c8/").expect("scan");
    assert_eq!(
        whole.rows, listed,
        "C8 limit: a page as large as the prefix holds every live key, in scan_prefix's order"
    );
    assert_eq!(
        whole.resume, None,
        "C8 resume: a page that reached the end of the range has no token"
    );

    let token = first.resume.as_ref().expect("a token");
    assert_eq!(
        keys_of(&page(store, c8_scan(2).resume(token))),
        ["c8/b", "c8/c"],
        "C8 resume: the token must resume exactly after the last key of the page"
    );
    assert_eq!(
        keys_of(&page(store, from(Bound::Excluded("c8/c"), 1))),
        ["c8/d"],
        "C8 lower: an excluded lower key is not returned"
    );
    for before in [Bound::Included("c8"), Bound::Excluded("c8.z")] {
        assert_eq!(
            keys_of(&page(store, from(before, 1))),
            ["c8/"],
            "C8 lower: a lower bound before the prefix begins at the prefix ({before:?})"
        );
    }
    assert_eq!(
        keys_of(&page(store, from(Bound::Excluded("c8/d"), 4))),
        ["c8/\u{e9}"],
        "C8 prefix: the page ends with the prefix's last key in byte order"
    );
    assert_pages(store, c8_scan(1), &C8_KEYS, "C8 prefix");
}

/// C8 upper: a forward scan stops at its upper bound, clipped to the prefix, and bounds
/// that cross give an empty page.
pub(super) fn c8_upper<S: Store>(store: &S) {
    let until = |upper, limit| ScanRange {
        upper,
        ..c8_scan(limit)
    };
    let included = page(store, until(Bound::Included("c8/b"), 8));
    assert_eq!(
        keys_of(&included),
        ["c8/", "c8/a", "c8/b"],
        "C8 upper: an included upper key is the last row"
    );
    assert_eq!(
        included.resume, None,
        "C8 upper: a page that reached the upper bound has no token"
    );
    assert_eq!(
        keys_of(&page(store, until(Bound::Excluded("c8/b"), 8))),
        ["c8/", "c8/a"],
        "C8 upper: an excluded upper key is not returned"
    );
    assert_pages(
        store,
        until(Bound::Included("c8/b"), 1),
        &C8_KEYS[..3],
        "C8 upper",
    );
    assert_eq!(
        keys_of(&page(store, until(Bound::Included("c9"), 8))),
        C8_KEYS,
        "C8 upper: an upper bound past the prefix stops at the prefix"
    );
    // Bounds that leave no key: before the prefix, crossed, and one key excluded.
    let empty = [
        (Bound::Unbounded, Bound::Included("c8.z")),
        (Bound::Included("c8/d"), Bound::Included("c8/b")),
        (Bound::Excluded("c8/b"), Bound::Excluded("c8/b")),
        (Bound::Included("c8/b"), Bound::Excluded("c8/b")),
        (Bound::Excluded("c8/b"), Bound::Included("c8/b")),
    ];
    for (lower, upper) in empty {
        for direction in [Direction::Forward, Direction::Reverse] {
            let none = page(
                store,
                ScanRange {
                    lower,
                    upper,
                    direction,
                    ..c8_scan(8)
                },
            );
            assert_eq!(
                (keys_of(&none), none.resume.as_ref()),
                (Vec::<&str>::new(), None),
                "C8 range: bounds {lower:?}..{upper:?} hold no key, so the page is empty and final"
            );
        }
    }
    let one = ScanRange {
        lower: Bound::Included("c8/b"),
        upper: Bound::Included("c8/b"),
        ..c8_scan(8)
    };
    assert_eq!(
        keys_of(&page(store, one)),
        ["c8/b"],
        "C8 range: equal included bounds hold their key"
    );
}

/// C8 reverse: descending order from the upper bound, a token that resumes below the
/// last key, and the lower bound and prefix as in a forward scan.
pub(super) fn c8_reverse<S: Store>(store: &S) {
    let first = page(store, c8_scan(2).reverse());
    assert_eq!(
        keys_of(&first),
        ["c8/\u{e9}", "c8/d"],
        "C8 reverse: a reverse page holds the last keys of the range, descending"
    );
    let token = first
        .resume
        .as_ref()
        .expect("C8 reverse: a full page has a token");
    assert_eq!(
        keys_of(&page(store, c8_scan(2).reverse().resume(token))),
        ["c8/c", "c8/b"],
        "C8 reverse resume: the token resumes just below the last key, and an expired key does not shorten the page"
    );
    let at_or_before = ScanRange {
        upper: Bound::Included("c8/bz"),
        ..c8_scan(1).reverse()
    };
    assert_eq!(
        keys_of(&page(store, at_or_before)),
        ["c8/b"],
        "C8 reverse: limit 1 under an included upper bound is the last live key at or before it"
    );
    let down_to = ScanRange {
        lower: Bound::Included("c8/c"),
        ..c8_scan(8).reverse()
    };
    let stopped = page(store, down_to);
    assert_eq!(
        keys_of(&stopped),
        ["c8/\u{e9}", "c8/d", "c8/c"],
        "C8 reverse: a reverse scan stops at its lower bound"
    );
    assert_eq!(
        stopped.resume, None,
        "C8 reverse: a page that reached the lower bound has no token"
    );
    let descending: Vec<&str> = C8_KEYS.iter().rev().copied().collect();
    assert_pages(
        store,
        c8_scan(1).reverse(),
        &descending,
        "C8 reverse paging",
    );
}

/// C8 concurrent: commits between pages, behind the token, ahead of it, and a delete and
/// an update ahead of it.
pub(super) fn c8_concurrent<S: Store>(store: &S) {
    let first = page(store, c8_scan(3));
    assert_eq!(
        keys_of(&first),
        ["c8/", "c8/a", "c8/b"],
        "C8 limit: page of 3"
    );
    run(
        store,
        Txn::new()
            .put("c8/aa", "behind", Cond::Absent)
            .put("c8/bz", "ahead", Cond::Absent)
            .put("c8/c", "updated", Cond::Any)
            .delete("c8/d", Cond::Any),
    )
    .expect("the concurrent commit applies");
    let token = first.resume.as_ref().expect("a token");
    let rest = page(store, c8_scan(8).resume(token));
    assert_eq!(
        keys_of(&rest),
        ["c8/bz", "c8/c", "c8/\u{e9}"],
        "C8 concurrent keys: the next page holds keys committed ahead of the token, not behind it, and not a deleted key"
    );
    assert_eq!(
        rest.rows[1].1.value, b"updated",
        "C8 concurrent value: a page shows the value committed before it was read"
    );
}

/// C8 empty prefix: every key is under it and no string follows them all, so the span
/// has no upper bound and a scan runs to the end of the store, or from it in reverse.
pub(super) fn c8_empty_prefix<S: Store>(store: &S) {
    let everything = ScanRange::new("", NonZeroUsize::new(8).expect("positive"));
    let tail = ScanRange {
        lower: Bound::Excluded("c8/\u{e9}"),
        ..everything
    };
    assert_eq!(
        keys_of(&page(store, tail)),
        ["c80", "c8~"],
        "C8 empty prefix: a forward scan runs from its lower bound to the last key in the store"
    );
    let last = ScanRange {
        limit: NonZeroUsize::MIN,
        ..everything.reverse()
    };
    assert_eq!(
        keys_of(&page(store, last)),
        ["c8~"],
        "C8 empty prefix: a reverse scan begins at the last key in the store"
    );
}
