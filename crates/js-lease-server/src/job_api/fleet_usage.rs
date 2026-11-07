//! `GetUsage` over its two sources, as one listing: the usage store's records of leases'
//! bookings ([`super::SinkHistory`]), and the records of jobs' allocations that the job store
//! keeps beside its tables ([`StoreHistory`]: the `usage/ended/` row each free of a job's
//! allocation writes in the same commit). Both are under the same keys and in the same
//! layout, so each source is read with the same query and the two pages are merged.
//!
//! **Order.** Records come oldest end first, then by id; a lease's record and a job's that
//! share both their end and their id (a job named like a lease id, ended in the same
//! millisecond) come lease first. So a record's place in the listing is its [`Cursor`]: its
//! key and its source.
//!
//! **Paging.** A page asks each source for up to the page's size of the records after the
//! cursor, merges the two in order and cuts the result to the page's size. The listing
//! continues when anything was cut or when either source had more, and the next cursor is the
//! last record kept. Each source then starts strictly after the cursor's key, which is right
//! for both unless the cursor is a lease's record: a job's record with the same key comes
//! after it and has not been listed, so the job store is also asked for that one record.
//! The filters apply in each source before the size counts.
//!
//! **Tokens.** A page token is the cursor's text: a lease's record writes its key's text
//! ([`EndedKey::text`], `<end, 20 digits>/<id>`), the token every answer gave before job
//! records were listed, and a job's record writes the same followed by `/job`. An allocation
//! id holds no `/`, so the two never read as each other.
//!
//! A source that cannot be read fails the call: a page is never answered from one source
//! alone when the other was asked.

use std::num::NonZeroUsize;
use std::sync::Arc;

use js_state::Store;
use js_usage::{EndedHistory, EndedKey, EndedPage, EndedQuery, EndedRow};

use super::History;
use crate::usage::UsageFault;
use crate::util::blocking;

/// What a job's record adds to its key's text in a token.
const JOB_SUFFIX: &str = "job";

/// Where a record comes from. A lease's record sorts before a job's with the same key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Source {
    /// The usage store's record of a lease's booking.
    Lease,
    /// The job store's record of a job's allocation.
    Job,
}

/// Where a record is in the listing: its key, then its source.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Cursor {
    /// The record's end and id.
    pub(super) key: EndedKey,
    /// The store it comes from.
    pub(super) source: Source,
}

impl Cursor {
    /// The cursor of `row`, read from `source`.
    fn of(row: &EndedRow, source: Source) -> Cursor {
        Cursor {
            key: EndedKey::of(row),
            source,
        }
    }

    /// The cursor as a page token.
    pub(super) fn text(&self) -> String {
        match self.source {
            Source::Lease => self.key.text(),
            Source::Job => format!("{}/{JOB_SUFFIX}", self.key.text()),
        }
    }

    /// The cursor whose token ([`Cursor::text`]) is `text`, or `None` for a token no answer
    /// gives.
    pub(super) fn parse(text: &str) -> Option<Cursor> {
        let (key, source) = match text.rsplit_once('/') {
            Some((key, JOB_SUFFIX)) if key.contains('/') => (key, Source::Job),
            _ => (text, Source::Lease),
        };
        let key = EndedKey::parse(key)?;
        Some(Cursor { key, source })
    }
}

/// The two sources of the listing; `None` for one the server does not hold (the usage store
/// with `--usage-off`).
pub(super) struct Sources<'a> {
    /// The usage store's records of leases.
    pub(super) leases: Option<&'a dyn History>,
    /// The job store's records of jobs.
    pub(super) jobs: Option<&'a dyn History>,
}

/// One page of the listing: its records, each with its cursor, and the cursor of the next
/// page (`None` on the last).
pub(super) struct Page {
    /// The records, in listing order.
    pub(super) rows: Vec<EndedRow>,
    /// The last record's cursor, when a record follows it.
    pub(super) next: Option<Cursor>,
}

/// One page of the records `query` matches after `after`, from both `sources`: at most
/// `query.limit`, in listing order. `query.after` is not read: `after` is the cursor.
///
/// # Errors
/// The [`UsageFault`] of the first source that cannot be read.
pub(super) async fn page(
    sources: &Sources<'_>,
    query: EndedQuery,
    after: Option<Cursor>,
) -> Result<Page, UsageFault> {
    let limit = query.limit.get();
    let strictly = EndedQuery {
        after: after.as_ref().map(|cursor| cursor.key.clone()),
        ..query.clone()
    };
    let mut rows: Vec<(Cursor, EndedRow)> = Vec::new();
    let mut more = false;
    let named = [(Source::Lease, sources.leases), (Source::Job, sources.jobs)];
    for (source, history) in named {
        let Some(history) = history else {
            continue;
        };
        // After a lease's cursor, a job's record of the same key follows it.
        let behind = after.as_ref().filter(|cursor| cursor.source < source);
        if let Some(same) = behind.and_then(|cursor| same_key(&query, &cursor.key)) {
            let found = history.page(same).await?;
            rows.extend(
                found
                    .rows
                    .into_iter()
                    .map(|row| (Cursor::of(&row, source), row)),
            );
        }
        let read: EndedPage = history.page(strictly.clone()).await?;
        more |= read.next.is_some();
        rows.extend(
            read.rows
                .into_iter()
                .map(|row| (Cursor::of(&row, source), row)),
        );
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    more |= rows.len() > limit;
    rows.truncate(limit);
    let next = more.then(|| rows.last().map(|(cursor, _)| cursor.clone()));
    Ok(Page {
        next: next.flatten(),
        rows: rows.into_iter().map(|(_, row)| row).collect(),
    })
}

/// The query of the one record with `key` that `query`'s window and filters let through;
/// `None` when they leave out every record with that key.
fn same_key(query: &EndedQuery, key: &EndedKey) -> Option<EndedQuery> {
    let from = key.ended_at_ms >= query.from_ms;
    let before = query
        .before_ms
        .is_none_or(|before| key.ended_at_ms < before);
    let id = query.id.as_ref().is_none_or(|id| *id == key.id);
    (from & before & id).then(|| EndedQuery {
        after: None,
        from_ms: key.ended_at_ms,
        // A key that ended at the last millisecond a `u64` holds: no record ends later.
        before_ms: key.ended_at_ms.checked_add(1),
        host: query.host.clone(),
        id: Some(key.id.clone()),
        limit: NonZeroUsize::MIN,
    })
}

/// The records of jobs' allocations, read from the state store under the job tables, where
/// each free of an allocation writes its `usage/ended/` row ([`js_usage::ended_writes`]).
pub(crate) struct StoreHistory(pub(crate) Arc<dyn Store>);

#[tonic::async_trait]
impl History for StoreHistory {
    async fn page(&self, query: EndedQuery) -> Result<EndedPage, UsageFault> {
        let store = Arc::clone(&self.0);
        blocking(move || {
            let read = EndedHistory::new(&*store).page(&query);
            read.map_err(|err| UsageFault(err.to_string()))
        })
        .await
    }
}
