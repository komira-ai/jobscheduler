//! The node series over a [`Store`], and the buffer that batches its writes.
//!
//! Unit of storage: one row per node per hour, key `usage/node/<host>/<hour>` with the
//! hour since the epoch zero-padded to 10 digits (so a bytewise scan is in time order),
//! value an [`encode_hour`] pack of up to 60 minutes. Neither backend trims its change
//! feed, and a per-minute row would add feed rows at write and again at expiry; a pack
//! rewritten every few minutes grows the feed by about a dozen rows per node-hour. The
//! write is a read-modify-write under the row's version, so a second writer fails loudly
//! with [`UsageError::Conflict`] rather than silently losing minutes (the server runs one
//! writer). Host ids are validated [`HostId`]s, never raw request text.

use std::collections::{BTreeMap, VecDeque};
use std::num::NonZeroUsize;
use std::ops::Bound;

use js_state::{Cond, ResumeToken, ScanRange, Store, Txn};
use js_types::HostId;

use crate::error::UsageError;
use crate::hour_of;
use crate::minute::{NodeMinute, decode_hour, encode_hour};

/// The key prefix of every node row.
pub const NODE_PREFIX: &str = "usage/node/";

/// The last hour a key can name (10 digits).
pub const MAX_NODE_HOUR: u64 = 9_999_999_999;

/// The last wall minute a series can hold: the last minute of [`MAX_NODE_HOUR`].
pub const MAX_NODE_MINUTE: u64 = MAX_NODE_HOUR * crate::MINUTES_PER_HOUR + 59;

/// The most minutes a [`NodeBuffer`] keeps per node while the store is down: one day.
pub const MAX_BUFFERED_PER_HOST: usize = 24 * 60;

/// Rows per page of a scan.
const PAGE: NonZeroUsize = NonZeroUsize::MIN.saturating_add(63);

/// The key prefix of one node's rows.
fn host_prefix(host: &HostId) -> String {
    format!("{NODE_PREFIX}{host}/")
}

/// The key of `host`'s row for `hour`.
#[must_use]
pub fn node_key(host: &HostId, hour: u64) -> String {
    format!("{}{hour:010}", host_prefix(host))
}

/// The hour a key under [`NODE_PREFIX`] names, or `None` for a key that is not a node row.
pub(crate) fn hour_of_key(key: &str) -> Option<u64> {
    let hour = key.rsplit('/').next().unwrap_or_default();
    if hour.len() == 10 && hour.bytes().all(|b| b.is_ascii_digit()) {
        hour.parse().ok()
    } else {
        None
    }
}

/// The per-node minute series, over a state store.
pub struct NodeSeries<'a> {
    pub(crate) store: &'a dyn Store,
}

impl<'a> NodeSeries<'a> {
    /// The series kept in `store`.
    #[must_use]
    pub fn new(store: &'a dyn Store) -> Self {
        Self { store }
    }

    /// Writes `rows` for `host`. A row for a minute already stored replaces it, so a
    /// repeated call is harmless; of two rows for one minute in `rows` the later wins. All
    /// the hours touched are written in one transaction.
    ///
    /// # Errors
    /// [`UsageError::MinuteOutOfRange`] before anything is read; [`UsageError::Corrupt`]
    /// when a stored hour does not read; [`UsageError::Conflict`] when a row changed
    /// between its read and the write; [`UsageError::Store`] when the store fails.
    pub fn append(&self, host: &HostId, rows: &[NodeMinute]) -> Result<(), UsageError> {
        let mut by_hour: BTreeMap<u64, BTreeMap<u64, NodeMinute>> = BTreeMap::new();
        for row in rows {
            if row.minute > MAX_NODE_MINUTE {
                return Err(UsageError::MinuteOutOfRange(row.minute));
            }
            by_hour
                .entry(hour_of(row.minute))
                .or_default()
                .insert(row.minute, *row);
        }
        if by_hour.is_empty() {
            return Ok(());
        }
        let mut txn = Txn::new();
        for (hour, fresh) in by_hour {
            let key = node_key(host, hour);
            let (mut merged, cond) = match self.store.get(&key)? {
                Some(old) => {
                    let stored =
                        decode_hour(hour, &old.value).map_err(|reason| UsageError::Corrupt {
                            key: key.clone(),
                            reason,
                        })?;
                    let map = stored.into_iter().map(|row| (row.minute, row)).collect();
                    (map, Cond::Version(old.version))
                }
                None => (BTreeMap::new(), Cond::Absent),
            };
            merged.extend(fresh);
            txn = txn.put(key, encode_hour(&merged), cond);
        }
        match self.store.txn(txn)? {
            Ok(_) => Ok(()),
            Err(conflict) => Err(UsageError::Conflict { key: conflict.key }),
        }
    }

    /// The stored minutes of `host` from wall minute `from` to `to`, both included, oldest
    /// first, at most `limit`. Minutes never stored are absent.
    ///
    /// # Errors
    /// [`UsageError::Corrupt`] when a stored hour in the range does not read;
    /// [`UsageError::Store`] when the store fails.
    pub fn range(
        &self,
        host: &HostId,
        from: u64,
        to: u64,
        limit: NonZeroUsize,
    ) -> Result<Vec<NodeMinute>, UsageError> {
        let to = to.min(MAX_NODE_MINUTE);
        if from > to {
            return Ok(Vec::new());
        }
        let prefix = host_prefix(host);
        let lower = node_key(host, hour_of(from));
        let upper = node_key(host, hour_of(to));
        let mut out = Vec::new();
        let mut token: Option<ResumeToken> = None;
        loop {
            let mut scan = ScanRange {
                lower: Bound::Included(&lower),
                upper: Bound::Included(&upper),
                ..ScanRange::new(&prefix, PAGE)
            };
            if let Some(token) = &token {
                scan = scan.resume(token);
            }
            let page = self.store.scan_range(scan)?;
            for (key, value) in &page.rows {
                // A key under the prefix that is not an hour row is not ours to read.
                let Some(hour) = hour_of_key(key) else {
                    continue;
                };
                let rows =
                    decode_hour(hour, &value.value).map_err(|reason| UsageError::Corrupt {
                        key: key.clone(),
                        reason,
                    })?;
                out.extend(rows.into_iter().filter(|r| (from..=to).contains(&r.minute)));
            }
            match page.resume {
                Some(next) if out.len() < limit.get() => token = Some(next),
                _ => break,
            }
        }
        out.truncate(limit.get());
        Ok(out)
    }
}

/// Node minutes waiting to be written. The server's tick moves due minutes in with
/// [`NodeBuffer::push`] and writes them every few minutes with [`NodeBuffer::flush`]; a
/// minute leaves the buffer only when its write succeeded.
#[derive(Debug, Default)]
pub struct NodeBuffer {
    rows: BTreeMap<HostId, VecDeque<NodeMinute>>,
    dropped: u64,
}

impl NodeBuffer {
    /// An empty buffer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a minute. When the node already has [`MAX_BUFFERED_PER_HOST`] waiting, the
    /// oldest is dropped and counted.
    pub fn push(&mut self, host: HostId, row: NodeMinute) {
        let queue = self.rows.entry(host).or_default();
        queue.push_back(row);
        if queue.len() > MAX_BUFFERED_PER_HOST {
            queue.pop_front();
            self.dropped += 1;
        }
    }

    /// How many minutes wait, all nodes together.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rows.values().map(VecDeque::len).sum()
    }

    /// Whether nothing waits.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// How many minutes were dropped for the per-node cap.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Writes every node's waiting minutes with [`NodeSeries::append`] and returns how many
    /// were written. A node whose write fails keeps its minutes for the next flush, and the
    /// other nodes are still written; the first error is returned.
    ///
    /// # Errors
    /// The first [`UsageError`] of any node's write.
    pub fn flush(&mut self, series: &NodeSeries<'_>) -> Result<usize, UsageError> {
        let mut written = 0;
        let mut first_error = None;
        let hosts: Vec<HostId> = self.rows.keys().cloned().collect();
        for host in hosts {
            let rows: Vec<NodeMinute> = self
                .rows
                .get(&host)
                .map(|queue| queue.iter().copied().collect())
                .unwrap_or_default();
            match series.append(&host, &rows) {
                Ok(()) => {
                    written += rows.len();
                    self.rows.remove(&host);
                }
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        first_error.map_or(Ok(written), Err)
    }
}
