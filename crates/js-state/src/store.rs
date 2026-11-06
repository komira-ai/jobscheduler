//! The state store trait.

use std::cmp::Ordering;
use std::num::NonZeroUsize;
use std::ops::Bound;

use crate::clock::StoreTime;
use crate::types::{
    Changes, Commit, CommitStats, Conflict, ResumeToken, Revision, ScanPage, ScanRange, StoreError,
    Txn, Versioned,
};

/// A durable, ordered key-value store with transactions, counters, time-to-live keys and
/// a change feed. The contract is clauses C1 to C8 of the architecture design; the
/// `conformance` module (feature `test-support`) is the executable form every backend
/// must pass.
///
/// # Expiry
/// A key put with a time to live is live while the store's clock reads strictly before
/// its `expires_at`, and expired from the reading that equals it: at exactly
/// `expires_at` a read does not see the key, a scan does not return it, `Cond::Absent`
/// holds and a `Cond::Version` on it does not, and [`Store::reap_expired`] removes it. So
/// a key put with a ttl of `d` at reading `t` is live for `t..t + d` and gone at `t + d`.
/// Every backend judges this the same way (C4 checks the boundary).
///
/// Calls block until done; the SQLite backend's writer thread and the Postgres backend's
/// connections are behind them. A caller on an async runtime calls from a blocking task.
pub trait Store: Send + Sync {
    /// The group-commit entry point. Applies each transaction in order, each atomically
    /// and on its own precondition, and returns one result per transaction in the same
    /// order. The batch is made durable with one sync, and every result is returned only
    /// after that sync (C6, C7). A backend with a writer thread feeds the transactions
    /// of concurrent [`Store::txn`] calls through the same path.
    ///
    /// # Errors
    /// [`StoreError`] when the backend fails; no result in the batch is then durable.
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError>;

    /// The live value of `key`. An expired key (its clock reading is at or past
    /// `expires_at`, see the type's Expiry rules) is absent whether or not it was reaped.
    ///
    /// # Errors
    /// [`StoreError`] when the backend fails.
    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError>;

    /// Every live key that starts with `prefix`, in key order.
    ///
    /// # Errors
    /// [`StoreError`] when the backend fails.
    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError>;

    /// A page of the live keys under `scan.prefix` that lie within `scan.lower` and
    /// `scan.upper`, in the key order of [`Store::scan_prefix`] (bytewise) or its
    /// reverse, at most `scan.limit` of them (C8). A bound outside the prefix is clipped
    /// to it, and bounds that cross give an empty page. [`ScanPage::resume`] is set when
    /// a live key of the range follows the last row in the scan's direction, and
    /// [`crate::ScanRange::resume`] makes the request for the next page, which begins
    /// exactly past that row.
    ///
    /// Under concurrent commits each page is read from one snapshot of committed state:
    /// it holds every transaction acknowledged before the call and none still in flight,
    /// each one whole or not at all. Pages are not one snapshot. A key committed behind
    /// the token after its page is not returned; one ahead of it is; a key deleted or
    /// expired before its page is read is absent; a key updated between pages shows the
    /// value of the page that returns it. So every key live in the range for the whole
    /// scan is returned exactly once, and the keys of a scan strictly ascend (or, in
    /// reverse, strictly descend).
    ///
    /// # Errors
    /// [`StoreError`] when the backend fails.
    fn scan_range(&self, scan: ScanRange<'_>) -> Result<ScanPage, StoreError>;

    /// At most `limit` feed rows with a revision after `after`, in revision order, never
    /// past a revision that can still commit (C5).
    ///
    /// A cursor below the feed's floor (the highest revision [`Store::trim_feed`] has
    /// removed through; 0 for a feed never trimmed) is refused with
    /// [`StoreError::StaleCursor`] instead of being answered with rows that skip the
    /// trimmed ones. A read at or past the floor is answered as if no trim had run.
    ///
    /// On Postgres, a row of a kind this build does not know (a newer release wrote it) is
    /// logged and left out of `rows`, though it counts toward `limit` and the cursor passes
    /// it: a page can hold fewer rows than the limit, or none, and its cursor still moves on.
    ///
    /// # Errors
    /// [`StoreError::StaleCursor`] when `after` is below the floor; [`StoreError`] when
    /// the backend fails.
    fn changes(&self, after: Revision, limit: usize) -> Result<Changes, StoreError>;

    /// Deletes every feed row whose revision is at or below `min(before, h)`, where `h` is
    /// the highest revision that can no longer change (the committed head, or one below
    /// the first revision still to commit: the horizon [`Store::changes`] reads to), and
    /// returns how many rows it deleted. It never deletes a row above a revision that
    /// can still commit. It writes no feed row and changes no key. The feed's floor
    /// rises to `min(before, h)` and never falls, so trimming again at or below the
    /// floor deletes nothing, and a cursor at or past the floor reads on as before.
    ///
    /// # Errors
    /// [`StoreError`] when the backend fails.
    fn trim_feed(&self, before: Revision) -> Result<u64, StoreError>;

    /// Removes every key the store's clock has expired, writing an `Expire` row for each,
    /// and returns how many. The reaper task calls this; reads do not wait for it (C4).
    ///
    /// # Errors
    /// [`StoreError`] when the backend fails.
    fn reap_expired(&self) -> Result<usize, StoreError>;

    /// The commit counters.
    fn commit_stats(&self) -> CommitStats;

    /// The store's clock reading, the one expiry is judged by.
    fn now(&self) -> StoreTime;

    /// Applies one transaction: a batch of one.
    ///
    /// # Errors
    /// [`StoreError`] when the backend fails.
    fn txn(&self, txn: Txn) -> Result<Result<Commit, Conflict>, StoreError> {
        // A backend returns one result per transaction; an empty answer is its bug.
        Ok(self.apply_batch(vec![txn])?.swap_remove(0))
    }
}

/// A shared handle to any store is a store: every call goes to the store it holds. A type
/// generic over `S: Store` that keeps an `Arc<S>` (a typed table over the store) can then run
/// on a store chosen at start, as `Arc<dyn Store>`.
impl Store for std::sync::Arc<dyn Store> {
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        (**self).apply_batch(batch)
    }

    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        (**self).get(key)
    }

    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        (**self).scan_prefix(prefix)
    }

    fn scan_range(&self, scan: ScanRange<'_>) -> Result<ScanPage, StoreError> {
        (**self).scan_range(scan)
    }

    fn changes(&self, after: Revision, limit: usize) -> Result<Changes, StoreError> {
        (**self).changes(after, limit)
    }

    fn trim_feed(&self, before: Revision) -> Result<u64, StoreError> {
        (**self).trim_feed(before)
    }

    fn reap_expired(&self) -> Result<usize, StoreError> {
        (**self).reap_expired()
    }

    fn commit_stats(&self) -> CommitStats {
        (**self).commit_stats()
    }

    fn now(&self) -> StoreTime {
        (**self).now()
    }

    fn txn(&self, txn: Txn) -> Result<Result<Commit, Conflict>, StoreError> {
        (**self).txn(txn)
    }
}

/// The keys a C8 request may return: its bounds clipped to its prefix. The lower edge is
/// always a key, since every key under the prefix is at or after the prefix itself; the
/// upper is unbounded only when no string follows every key under the prefix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Span {
    pub lower: Lower,
    pub upper: Bound<String>,
}

/// The lower edge of a [`Span`]: a key, and whether the key itself is in the span.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Lower {
    pub key: String,
    pub included: bool,
}

impl Span {
    /// The span of `scan`, or `None` when no key can lie in it.
    pub(crate) fn of(scan: &ScanRange<'_>) -> Option<Span> {
        let (low, included) = tighter((scan.prefix, true), scan.lower, Ordering::Greater);
        let upper = match successor(scan.prefix) {
            Some(ceiling) => match tighter((&ceiling, false), scan.upper, Ordering::Less) {
                (high, true) => Bound::Included(high.to_owned()),
                (high, false) => Bound::Excluded(high.to_owned()),
            },
            None => scan.upper.map(str::to_owned),
        };
        let empty = match &upper {
            Bound::Included(high) if included => low > high.as_str(),
            Bound::Included(high) | Bound::Excluded(high) => low >= high.as_str(),
            Bound::Unbounded => false,
        };
        (!empty).then(|| Span {
            lower: Lower {
                key: low.to_owned(),
                included,
            },
            upper,
        })
    }

    /// The span as borrowed bounds, the form `BTreeMap::range` takes (the fake's).
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn bounds(&self) -> (Bound<&str>, Bound<&str>) {
        let lower = if self.lower.included {
            Bound::Included(self.lower.key.as_str())
        } else {
            Bound::Excluded(self.lower.key.as_str())
        };
        (lower, self.upper.as_ref().map(String::as_str))
    }
}

/// Of an edge that is a key, `(key, included)`, and a request's bound on the same side,
/// the one that admits fewer keys: the key further in the `inward` direction, and at a
/// tie the excluded one.
fn tighter<'a>(base: (&'a str, bool), other: Bound<&'a str>, inward: Ordering) -> (&'a str, bool) {
    match other {
        Bound::Included(theirs) if theirs.cmp(base.0) == inward => (theirs, true),
        Bound::Excluded(theirs) if theirs.cmp(base.0) != inward.reverse() => (theirs, false),
        _ => base,
    }
}

/// The least string greater than every string that starts with `prefix`: the prefix
/// with its last character replaced by the next one, after dropping trailing characters
/// that have none. `None` when there is no such string (an empty prefix, or one of only
/// `char::MAX`). Bytewise order on UTF-8 is code point order, so this holds for the
/// byte order every backend sorts keys by.
pub(crate) fn successor(prefix: &str) -> Option<String> {
    let mut chars: Vec<char> = prefix.chars().collect();
    while let Some(last) = chars.pop() {
        let next = match last {
            '\u{d7ff}' => Some('\u{e000}'),
            other => char::from_u32(u32::from(other) + 1),
        };
        if let Some(next) = next {
            chars.push(next);
            return Some(chars.into_iter().collect());
        }
    }
    None
}

/// A C8 page from up to `limit + 1` live rows read in the scan's order: the first
/// `limit`, and a resume token when there was one more.
pub(crate) fn page_of(mut rows: Vec<(String, Versioned)>, limit: NonZeroUsize) -> ScanPage {
    let more = rows.len() > limit.get();
    rows.truncate(limit.get());
    let resume = match rows.last() {
        Some((key, _)) if more => Some(ResumeToken::after(key.clone())),
        _ => None,
    };
    ScanPage { rows, resume }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;
    use std::ops::Bound;
    use std::sync::Arc;
    use std::time::Duration;

    use super::{Span, Store, successor};
    use crate::clock::{Clock, ManualClock};
    use crate::fake::{Fake, Flaw};
    use crate::types::{Cond, Revision, ScanRange, Txn};

    /// A shared handle answers every call as the store it holds: a write through the handle
    /// is read back through the store and through the handle alike, the scans, the feed,
    /// the reaper, the counters and the clock are the store's. It catches a forwarding
    /// method that answers from somewhere else (a default, another method) instead.
    #[test]
    fn a_shared_handle_is_the_store_it_holds() {
        let clock = Arc::new(ManualClock::default());
        let inner: Arc<dyn Store> = Arc::new(Fake::with_flaw(clock.clone(), Flaw::None));
        let handle: Arc<dyn Store> = Arc::clone(&inner);
        let through = |txn| <Arc<dyn Store> as Store>::txn(&handle, txn);
        through(Txn::new().put("k/a", b"1".to_vec(), Cond::Absent))
            .expect("store")
            .expect("commit");
        let batch = vec![
            Txn::new().put("k/b", b"2".to_vec(), Cond::Absent),
            Txn::new().put_ttl("k/c", b"3".to_vec(), Duration::from_secs(1), Cond::Absent),
        ];
        let results = <Arc<dyn Store> as Store>::apply_batch(&handle, batch).expect("store");
        assert!(results.iter().all(Result::is_ok));

        let got = <Arc<dyn Store> as Store>::get(&handle, "k/a").expect("store");
        assert_eq!(got, inner.get("k/a").expect("store"));
        assert_eq!(got.map(|row| row.value), Some(b"1".to_vec()));
        let keys = |rows: Vec<(String, crate::types::Versioned)>| {
            rows.into_iter().map(|(key, _)| key).collect::<Vec<_>>()
        };
        let prefix = <Arc<dyn Store> as Store>::scan_prefix(&handle, "k/").expect("store");
        assert_eq!(keys(prefix), ["k/a", "k/b", "k/c"]);
        let page =
            <Arc<dyn Store> as Store>::scan_range(&handle, ScanRange::new("k/", NonZeroUsize::MIN))
                .expect("store");
        assert_eq!(keys(page.rows), ["k/a"]);
        assert!(page.resume.is_some());
        let feed = <Arc<dyn Store> as Store>::changes(&handle, Revision(0), 10).expect("store");
        assert_eq!(feed, inner.changes(Revision(0), 10).expect("store"));
        assert_eq!(feed.rows.len(), 3);
        assert_eq!(<Arc<dyn Store> as Store>::commit_stats(&handle).txns, 3);

        let before = <Arc<dyn Store> as Store>::now(&handle);
        clock.advance(Duration::from_secs(1));
        let after = <Arc<dyn Store> as Store>::now(&handle);
        assert_eq!(after, clock.now());
        assert!(after > before);
        assert_eq!(<Arc<dyn Store> as Store>::reap_expired(&handle), Ok(1));
        assert_eq!(inner.get("k/c").expect("store"), None);

        // The feed holds four rows now (three puts and the expiry); a trim through the
        // handle removes the first two from the store underneath.
        assert_eq!(
            <Arc<dyn Store> as Store>::trim_feed(&handle, Revision(2)),
            Ok(2)
        );
        let left = inner.changes(Revision(2), 10).expect("store");
        assert_eq!(left.rows.len(), 2);
    }

    // Catches a successor that steps into the surrogate gap (no char there, so the
    // character would be dropped and the bound would jump a whole level) or that keeps
    // a trailing char::MAX it cannot step.
    #[test]
    fn the_successor_of_a_prefix_is_the_next_string_past_everything_under_it() {
        assert_eq!(successor("c8/").as_deref(), Some("c80"));
        assert_eq!(successor("a\u{d7ff}").as_deref(), Some("a\u{e000}"));
        assert_eq!(successor("a\u{10ffff}").as_deref(), Some("b"));
        assert_eq!(successor("\u{10ffff}\u{10ffff}"), None);
        assert_eq!(successor(""), None);
    }

    // Catches a span that invents an upper bound for a prefix with no successor: the
    // empty prefix is every key, so only the request's own upper bound applies.
    #[test]
    fn a_prefix_with_no_successor_leaves_the_upper_bound_to_the_request() {
        let limit = NonZeroUsize::MIN;
        let all = Span::of(&ScanRange::new("", limit)).expect("a span");
        assert_eq!(all.bounds(), (Bound::Included(""), Bound::Unbounded));
        let until = ScanRange {
            upper: Bound::Excluded("m"),
            ..ScanRange::new("", limit)
        };
        let span = Span::of(&until).expect("a span");
        assert_eq!(span.bounds(), (Bound::Included(""), Bound::Excluded("m")));
    }
}
