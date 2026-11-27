//! The rows the job-store verbs move, and what they say of them: a count and a checksum per
//! key prefix ([`Tally`]), the plan of a copy ([`plan`]) and its write ([`write`]), and the
//! export of a family of prefixes into a file ([`export`]).
//!
//! Every read pages through the store in key order, [`PAGE`] rows at a time, and every write is
//! one transaction per page whose puts are conditional on the key being absent, so neither
//! verb holds a table in memory and neither replaces a row it did not read as missing.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::num::NonZeroUsize;

use js_state::{Cond, ResumeToken, ScanRange, Store, StoreError, StoreTime, Txn, Versioned};
use sha2::{Digest as _, Sha256};

/// How many rows one read of a scan returns and one write commits.
pub(crate) const PAGE: NonZeroUsize = NonZeroUsize::MIN.saturating_add(511);

/// The prefixes of the job store's rows: what `<state file>.jobs` holds without a store
/// setting, and what the export writes to the job store's file. The `usage/ended` rows are
/// here because the job store writes them in the transaction that frees an allocation, so
/// without a store setting they are in the job store's file, where `GetUsage` reads them.
pub(crate) const JOB_ROWS: &[&str] = &[
    js_jobs::keys::ALLOCATION,
    js_jobs::keys::ATTEMPT,
    js_jobs::keys::CALLBACK,
    js_jobs::keys::CORDON,
    js_jobs::keys::DISPATCH,
    js_jobs::keys::HOSTBOOK,
    js_jobs::keys::HOSTMARK,
    "idx/",
    js_jobs::keys::JOB,
    js_jobs::keys::MARK,
    js_jobs::keys::CORDONS_RESTORED,
    js_jobs::keys::SCHEMA,
    js_jobs::keys::NODE,
    js_jobs::keys::PERIODIC,
    js_jobs::keys::POOL,
    js_jobs::keys::TAINT,
    js_usage::ENDED_PREFIX,
    js_usage::ENDED_ID_PREFIX,
];

/// The prefixes of the usage rows the usage file (`usage.db`, `--usage-db`) holds without a
/// store setting: node minutes, profiles and tails.
pub(crate) const USAGE_ROWS: &[&str] = &[
    js_usage::KEY_PREFIX,
    js_usage::store::NODE_PREFIX,
    js_usage::TAIL_PREFIX,
];

/// Why a copy or an export stopped.
#[derive(Debug, thiserror::Error)]
pub(crate) enum RowsError {
    /// A store could not be read or written.
    #[error("{0}")]
    Store(#[from] StoreError),
    /// A put conditional on its key being absent found the key there: another process wrote
    /// it after it was read as missing. The pages before it are written.
    #[error(
        "{0:?} was written by another process after it was read as missing: the rows before it \
         are written; run the dry run again"
    )]
    Raced(String),
}

/// What a set of rows holds, per key prefix: how many rows, and a SHA-256 over each row's key
/// and value in key order (each as its length, eight bytes big-endian, then its bytes). A
/// row's time to live is not in the checksum: a copy writes it again relative to the clock of
/// the store it writes, so two stores that hold the same rows have the same tally.
#[derive(Default)]
pub(crate) struct Tally {
    /// The groups, by prefix.
    groups: BTreeMap<&'static str, Group>,
    /// Rows of keys under no known prefix, by their first segment.
    others: BTreeMap<String, Group>,
}

/// One prefix of a [`Tally`].
#[derive(Default)]
struct Group {
    /// How many rows.
    rows: u64,
    /// The checksum so far.
    digest: Sha256,
}

impl Group {
    /// Counts the row `key`, `value`.
    fn add(&mut self, key: &str, value: &[u8]) {
        self.rows += 1;
        for bytes in [key.as_bytes(), value] {
            self.digest.update((bytes.len() as u64).to_be_bytes());
            self.digest.update(bytes);
        }
    }

    /// The line of the prefix `prefix`.
    fn line(&self, prefix: &str) -> String {
        let hex: String = self
            .digest
            .clone()
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        format!("  {prefix} rows={} sha256={hex}", self.rows)
    }
}

impl Tally {
    /// Counts the row `key`, `value` under its prefix: the first of [`JOB_ROWS`] and
    /// [`USAGE_ROWS`] the key starts with, else the key up to and with its first `/`.
    pub(crate) fn add(&mut self, key: &str, value: &[u8]) {
        let known = JOB_ROWS
            .iter()
            .chain(USAGE_ROWS)
            .find(|p| key.starts_with(**p));
        match known {
            Some(prefix) => self.groups.entry(*prefix).or_default().add(key, value),
            None => {
                let first = key.find('/').map_or(key, |at| &key[..=at]);
                self.others
                    .entry(first.to_owned())
                    .or_default()
                    .add(key, value);
            }
        }
    }

    /// How many rows were counted.
    pub(crate) fn rows(&self) -> u64 {
        self.groups
            .values()
            .chain(self.others.values())
            .map(|group| group.rows)
            .sum()
    }

    /// The tally's lines, one per prefix in key order.
    pub(crate) fn lines(&self) -> Vec<String> {
        let mut lines: BTreeMap<&str, String> = BTreeMap::new();
        for (prefix, group) in &self.groups {
            lines.insert(*prefix, group.line(prefix));
        }
        for (prefix, group) in &self.others {
            lines.insert(prefix.as_str(), group.line(prefix));
        }
        lines.into_values().collect()
    }

    /// Writes [`Tally::lines`] to `out`, one per line.
    pub(crate) fn write_to(&self, out: &mut dyn Write) -> io::Result<()> {
        self.lines()
            .iter()
            .try_for_each(|line| writeln!(out, "{line}"))
    }
}

/// What a copy of a file's rows into a store would do.
#[derive(Default)]
pub(crate) struct Plan {
    /// The file's rows.
    pub(crate) tally: Tally,
    /// How many of them the store does not hold: the rows the write writes.
    pub(crate) missing: u64,
    /// How many of them the store holds with the same value.
    pub(crate) same: u64,
    /// The keys the store holds with another value, in key order.
    pub(crate) different: Vec<String>,
}

/// Reads every row of `source` and finds, for each, whether `target` holds it: not at all,
/// with the same value, or with another. Writes nothing.
pub(crate) fn plan(source: &dyn Store, target: &dyn Store) -> Result<Plan, RowsError> {
    let mut plan = Plan::default();
    pages(source, "", &mut |rows| {
        let held = held(target, rows)?;
        for (key, row) in rows {
            plan.tally.add(key, &row.value);
            match held.get(key) {
                None => plan.missing += 1,
                Some(there) if there.value == row.value => plan.same += 1,
                Some(_) => plan.different.push(key.clone()),
            }
        }
        Ok(())
    })?;
    Ok(plan)
}

/// Writes every row of `source` that `target` does not hold into `target`, a page per
/// transaction, each put conditional on its key being absent, with what is left of its time
/// to live on `source`'s clock; a row `target` holds is left as it is. Returns how many rows it wrote.
pub(crate) fn write(source: &dyn Store, target: &dyn Store) -> Result<u64, RowsError> {
    let mut wrote = 0;
    pages(source, "", &mut |rows| {
        let held = held(target, rows)?;
        let missing: Vec<&(String, Versioned)> = rows
            .iter()
            .filter(|(key, _)| !held.contains_key(key))
            .collect();
        for chunk in missing.chunks(PAGE.get()) {
            put_all((source, target), chunk.iter().copied())?;
            wrote += chunk.len() as u64;
        }
        Ok(())
    })?;
    Ok(wrote)
}

/// Writes every row of `store` under the prefixes `prefixes` into `file`, which holds none of
/// them, and returns their tally.
pub(crate) fn export(
    store: &dyn Store,
    prefixes: &[&str],
    file: &dyn Store,
) -> Result<Tally, RowsError> {
    let mut tally = Tally::default();
    for prefix in prefixes {
        pages(store, prefix, &mut |rows| {
            for chunk in rows.chunks(PAGE.get()) {
                chunk
                    .iter()
                    .for_each(|(key, row)| tally.add(key, &row.value));
                put_all((store, file), chunk.iter())?;
            }
            Ok(())
        })?;
    }
    Ok(tally)
}

/// The rows of `rows` that `target` holds, by key.
fn held(
    target: &dyn Store,
    rows: &[(String, Versioned)],
) -> Result<BTreeMap<String, Versioned>, StoreError> {
    let keys: Vec<String> = rows.iter().map(|(key, _)| key.clone()).collect();
    Ok(target.get_many(&keys)?.into_iter().collect())
}

/// Puts `rows`, read from `read`, into `target` in one transaction, each conditional on its
/// key being absent and with what is left of its time to live at `read`'s now: a row's end is
/// a time of the clock of the store it was read from.
fn put_all<'a>(
    (read, target): (&dyn Store, &dyn Store),
    rows: impl Iterator<Item = &'a (String, Versioned)>,
) -> Result<(), RowsError> {
    let now = read.now()?;
    let txn = rows.fold(Txn::new(), |txn, (key, row)| put(txn, key, row, now));
    target
        .txn(txn)?
        .map(|_| ())
        .map_err(|conflict| RowsError::Raced(conflict.key))
}

/// `txn` with the put of `row` at `key`, conditional on the key being absent; a row with an
/// end is written with the time from `now` to that end (none, when it is already past).
fn put(txn: Txn, key: &str, row: &Versioned, now: StoreTime) -> Txn {
    let value = row.value.clone();
    match row.expires_at {
        Some(end) => txn.put_ttl(key, value, end.since(now), Cond::Absent),
        None => txn.put(key, value, Cond::Absent),
    }
}

/// Calls `each` with every page of `store`'s live rows under `prefix`, in key order, until
/// the last page or the first error.
fn pages(
    store: &dyn Store,
    prefix: &str,
    each: &mut dyn FnMut(&[(String, Versioned)]) -> Result<(), RowsError>,
) -> Result<(), RowsError> {
    let mut after: Option<ResumeToken> = None;
    loop {
        let scan = ScanRange::new(prefix, PAGE);
        let scan = match after.as_ref() {
            Some(token) => scan.resume(token),
            None => scan,
        };
        let page = store.scan_range(scan)?;
        each(&page.rows)?;
        match page.resume {
            Some(token) => after = Some(token),
            None => return Ok(()),
        }
    }
}
