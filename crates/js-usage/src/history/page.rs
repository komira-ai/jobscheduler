//! A page of the history for a reader that comes back for the next one
//! ([`EndedHistory::page`]): the rows after a cursor, in a half-open window of ends, of one
//! host or one allocation when asked, with the cursor that reads the next page.
//!
//! The cursor is the key of the last row a page returned ([`EndedKey`]). Keys sort in time
//! order and are unique, so the next page starts strictly after it: no row is read twice
//! and none is skipped, also when rows are written between two pages (a row that ends
//! after the cursor is on a later page; one written behind it is not read by this
//! listing). The filters are applied before the limit counts, so a page is full whenever
//! enough rows match.

use std::num::NonZeroUsize;
use std::ops::Bound;

use js_state::{ResumeToken, ScanRange};
use js_types::HostId;

use super::{AllocationId, ENDED_PREFIX, EndedHistory, EndedRow, PAGE, decode, ended_key};
use crate::error::UsageError;

/// The digits of the end in a row's key.
const END_DIGITS: usize = 20;

/// Where a row is in the history: its end and its id, the two parts of its key. Keys
/// compare as their rows sort: by end, then by id.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EndedKey {
    /// When the allocation ended, in milliseconds since the Unix epoch.
    pub ended_at_ms: u64,
    /// The allocation.
    pub id: AllocationId,
}

impl EndedKey {
    /// The key of `row`.
    #[must_use]
    pub fn of(row: &EndedRow) -> Self {
        Self {
            ended_at_ms: row.actual_end,
            id: row.id.clone(),
        }
    }

    /// The key as text: the row's store key without its prefix, `<end, 20 digits>/<id>`.
    #[must_use]
    pub fn text(&self) -> String {
        format!("{:020}/{}", self.ended_at_ms, self.id)
    }

    /// The key whose text ([`EndedKey::text`]) is `text`, or `None` for a text that no key
    /// has: no `/`, an end that is not 20 decimal digits or does not fit in a `u64`, or an
    /// id that is not an [`AllocationId`].
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let (end, id) = text.split_once('/')?;
        let digits = (end.len() == END_DIGITS) & end.bytes().all(|b| b.is_ascii_digit());
        let ended_at_ms = digits.then(|| end.parse().ok()).flatten()?;
        let id = AllocationId::parse(id).ok()?;
        Some(Self { ended_at_ms, id })
    }

    /// The row's store key.
    fn key(&self) -> String {
        ended_key(self.ended_at_ms, &self.id)
    }
}

/// What a page of the history is asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EndedQuery {
    /// The last row of the previous page: the page starts after it. `None`: the first page.
    pub after: Option<EndedKey>,
    /// Only rows that ended at or after this time, in milliseconds since the Unix epoch.
    pub from_ms: u64,
    /// Only rows that ended before this time, in milliseconds since the Unix epoch. `None`:
    /// no upper bound.
    pub before_ms: Option<u64>,
    /// Only rows booked on this host. `None`: every host, and the rows with none.
    pub host: Option<HostId>,
    /// Only the row of this allocation. `None`: every allocation.
    pub id: Option<AllocationId>,
    /// The most rows the page holds.
    pub limit: NonZeroUsize,
}

impl EndedQuery {
    /// Whether `row` passes the host and allocation filters.
    fn wants(&self, row: &EndedRow) -> bool {
        let host = self
            .host
            .as_ref()
            .is_none_or(|host| row.host.as_ref() == Some(host));
        let id = self.id.as_ref().is_none_or(|id| row.id == *id);
        host & id
    }
}

/// One page of the history.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EndedPage {
    /// The rows, oldest end first (rows of one end in id order).
    pub rows: Vec<EndedRow>,
    /// The cursor of the next page, the key of the last row here; `None` when no row
    /// that the query matches follows.
    pub next: Option<EndedKey>,
}

impl EndedHistory<'_> {
    /// At most `query.limit` rows that `query` matches, after `query.after`, oldest end
    /// first, and the cursor of the next page when a matching row follows them. A row this
    /// build does not read, or whose key is not the one its end and id give, is left out
    /// and does not count towards the limit. It writes nothing.
    ///
    /// # Errors
    /// [`UsageError::Store`] when the store fails.
    pub fn page(&self, query: &EndedQuery) -> Result<EndedPage, UsageError> {
        let floor = format!("{ENDED_PREFIX}{:020}/", query.from_ms);
        let after = query.after.as_ref().map(EndedKey::key);
        // A cursor before the window's start reads from the start: a key of an end at
        // `from_ms` sorts after `floor`, since `/` sorts before every id byte.
        let lower = match after.as_deref().filter(|after| *after >= floor.as_str()) {
            Some(after) => Bound::Excluded(after),
            None => Bound::Included(floor.as_str()),
        };
        // Every key of an end before `before_ms` sorts before this one, and every key of
        // an end at `before_ms` or later after it.
        let ceiling = query
            .before_ms
            .map(|before| format!("{ENDED_PREFIX}{before:020}/"));
        let upper = match ceiling.as_deref() {
            Some(ceiling) => Bound::Excluded(ceiling),
            None => Bound::Unbounded,
        };
        let limit = query.limit.get();
        let mut rows = Vec::new();
        let mut token: Option<ResumeToken> = None;
        loop {
            let mut scan = ScanRange {
                lower,
                upper,
                ..ScanRange::new(ENDED_PREFIX, PAGE)
            };
            if let Some(token) = &token {
                scan = scan.resume(token);
            }
            let page = self.store.scan_range(scan)?;
            for (key, held) in &page.rows {
                let row = decode(&held.value).ok();
                let row = row.filter(|row| ended_key(row.actual_end, &row.id) == *key);
                rows.extend(row.filter(|row| query.wants(row)));
            }
            // One row past the limit says whether a next page has anything on it.
            match page.resume {
                Some(next) if rows.len() <= limit => token = Some(next),
                _ => break,
            }
        }
        let more = rows.len() > limit;
        rows.truncate(limit);
        let next = more.then(|| rows.last().map(EndedKey::of)).flatten();
        Ok(EndedPage { rows, next })
    }
}
