//! The node series over a [`Store`], and the buffer that batches its writes.
//!
//! Unit of storage: one row per node per hour, key `usage/node/<host>/<hour>` with the
//! hour since the epoch zero-padded to 10 digits (so a bytewise scan is in time order),
//! value an [`encode_hour`] pack of up to 60 minutes. Neither backend trims its change
//! feed, and a per-minute row would add feed rows at write and again at expiry; a pack
//! rewritten every few minutes grows the feed by about a dozen rows per node-hour. Host ids
//! are validated [`HostId`]s, never raw request text.
//!
//! **Several writers.** Several server instances may share the store, and a host that moves
//! from one to another within a minute has that minute measured by both, each over its part
//! of the minute. A series written as an instance ([`NodeSeries::with_instance`]) keeps
//! both: a minute the hour's pack does not hold yet goes into the pack, and a minute the
//! pack already holds with other figures goes into the instance's own row beside the pack,
//! `usage/node/<host>/<hour>/<instance>`, named by its [`InstanceTag`]. A minute the pack
//! holds with the same figures is not written again (a write of this instance that landed
//! though its reply was lost, or a part that adds nothing to the fold). A reader folds the
//! pack and every instance row of the hour into one minute: the higher mean CPU, working
//! set and lease count of the parts. Each part's CPU is the host's mean over that part, so
//! a sum would count the host twice, and a part read twice is taken once. A write, of the
//! pack and of the instance's row, is a read-modify-write under the row's version, so a
//! writer that loses a race fails with [`UsageError::Conflict`] and keeps its rows buffered
//! for its next flush. A series with no instance writes as the one writer: a minute stored
//! already is replaced in the pack, as a release before this one writes. A release before
//! reads the pack alone (its scan passes over the instance rows), so it misses only the
//! other instances' parts of such a minute.

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::num::NonZeroUsize;
use std::ops::Bound;

use js_state::{Cond, ResumeToken, ScanRange, Store, Txn};
use js_types::HostId;

use crate::error::UsageError;
use crate::minute::{NodeMinute, decode_hour, encode_hour};
use crate::{MINUTES_PER_HOUR, hour_of};

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

/// The key of the row of `instance`'s own minutes of `host` in `hour`, beside the hour's pack
/// ([`node_key`]).
#[must_use]
pub fn instance_key(host: &HostId, hour: u64, instance: &InstanceTag) -> String {
    format!("{}/{instance}", node_key(host, hour))
}

/// The name of one server instance's own rows of the node series: its start (wall
/// milliseconds since the epoch), its process id, and which instance of that process it is,
/// joined by `-`, so it is never a 10-digit hour and holds no `/`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InstanceTag(String);

impl InstanceTag {
    /// The tag of the instance started at `started_ms` in process `pid`, the `nth` the
    /// process started (0 for the first).
    #[must_use]
    pub fn started(started_ms: u64, pid: u32, nth: u32) -> Self {
        Self(format!("{started_ms}-{pid}-{nth}"))
    }
}

impl fmt::Display for InstanceTag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Whether `text` is an hour component of a key: 10 digits.
fn is_hour(text: &str) -> bool {
    text.len() == 10 && text.bytes().all(|b| b.is_ascii_digit())
}

/// Whether `text` is an [`InstanceTag`]: digits and at least one `-`.
fn is_tag(text: &str) -> bool {
    text.contains('-') && text.bytes().all(|b| b.is_ascii_digit() || b == b'-')
}

/// The hour a key under [`NODE_PREFIX`] names, a pack's (`<host>/<hour>`) or an instance
/// row's (`<host>/<hour>/<instance>`), or `None` for a key that is neither.
pub(crate) fn hour_of_key(key: &str) -> Option<u64> {
    let mut back = key.rsplit('/');
    let last = back.next().unwrap_or_default();
    let hour = if is_tag(last) {
        back.next().unwrap_or_default()
    } else {
        last
    };
    if is_hour(hour) {
        hour.parse().ok()
    } else {
        None
    }
}

/// One minute of a host as two instances measured it, each over its part of the minute: the
/// higher mean CPU, working set and lease count. The CPU of each part is the host's mean over
/// that part, so a sum would count the host twice; the larger never books the host below
/// what it ran. The fold is the same whichever comes first, and a part folded twice is
/// taken once.
fn fold(one: NodeMinute, other: NodeMinute) -> NodeMinute {
    NodeMinute {
        minute: one.minute,
        cpu_millis: one.cpu_millis.max(other.cpu_millis),
        ws_max_bytes: one.ws_max_bytes.max(other.ws_max_bytes),
        leases: one.leases.max(other.leases),
    }
}

/// The per-node minute series, over a state store.
pub struct NodeSeries<'a> {
    pub(crate) store: &'a dyn Store,
    /// The instance whose own rows a write keeps a minute the pack holds already in; `None`
    /// writes as the one writer.
    instance: Option<&'a InstanceTag>,
}

impl<'a> NodeSeries<'a> {
    /// The series kept in `store`, written as its one writer.
    #[must_use]
    pub fn new(store: &'a dyn Store) -> Self {
        Self {
            store,
            instance: None,
        }
    }

    /// The same series, written as the instance `instance`, one of several sharing the
    /// store (see the [module](self)).
    #[must_use]
    pub fn with_instance(self, instance: &'a InstanceTag) -> Self {
        Self {
            instance: Some(instance),
            ..self
        }
    }

    /// Writes `rows` for `host`. Of two rows for one minute in `rows` the later wins. With no
    /// instance a row for a minute already stored replaces it in the hour's pack, so a
    /// repeated call is harmless. As an instance a minute the pack does not hold goes into
    /// it, one the pack holds with other figures goes into the instance's own row of the
    /// hour (replacing what that row held for the minute), and one the pack holds with the
    /// same figures is not written. All the hours touched are written in one transaction,
    /// and none when nothing changes.
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
        let mut writes = 0;
        for (hour, fresh) in by_hour {
            let key = node_key(host, hour);
            let (mut pack, cond) = self.read_hour(&key, hour)?;
            let Some(instance) = self.instance else {
                pack.extend(fresh);
                txn = txn.put(key, encode_hour(&pack), cond);
                writes += 1;
                continue;
            };
            let mut own = BTreeMap::new();
            let mut grew = false;
            for (minute, row) in fresh {
                match pack.get(&minute) {
                    None => {
                        pack.insert(minute, row);
                        grew = true;
                    }
                    Some(held) if *held == row => {}
                    Some(_) => {
                        own.insert(minute, row);
                    }
                }
            }
            if grew {
                txn = txn.put(key, encode_hour(&pack), cond);
                writes += 1;
            }
            if !own.is_empty() {
                let key = instance_key(host, hour, instance);
                let (mut mine, cond) = self.read_hour(&key, hour)?;
                mine.extend(own);
                txn = txn.put(key, encode_hour(&mine), cond);
                writes += 1;
            }
        }
        if writes == 0 {
            return Ok(());
        }
        match self.store.txn(txn)? {
            Ok(_) => Ok(()),
            Err(conflict) => Err(UsageError::Conflict { key: conflict.key }),
        }
    }

    /// The minutes of the hour row at `key` (hour `hour`), by minute, and the condition a
    /// write of the row is made under: its version, or absent.
    fn read_hour(
        &self,
        key: &str,
        hour: u64,
    ) -> Result<(BTreeMap<u64, NodeMinute>, Cond), UsageError> {
        let Some(old) = self.store.get(key)? else {
            return Ok((BTreeMap::new(), Cond::Absent));
        };
        let stored = decode_hour(hour, &old.value).map_err(|reason| UsageError::Corrupt {
            key: key.to_owned(),
            reason,
        })?;
        let map = stored.into_iter().map(|row| (row.minute, row)).collect();
        Ok((map, Cond::Version(old.version)))
    }

    /// The stored minutes of `host` from wall minute `from` to `to`, both included, oldest
    /// first, at most `limit`, each folded from the hour's pack and every instance row of the
    /// hour that holds it (see the [module](self)). Minutes never stored are absent.
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
        // Past every instance row of the last hour (`/` sorts below `0`), and below the next
        // hour's pack.
        let upper = format!("{}0", node_key(host, hour_of(to)));
        let mut folded: BTreeMap<u64, NodeMinute> = BTreeMap::new();
        let mut token: Option<ResumeToken> = None;
        loop {
            let mut scan = ScanRange {
                lower: Bound::Included(&lower),
                upper: Bound::Excluded(&upper),
                ..ScanRange::new(&prefix, PAGE)
            };
            if let Some(token) = &token {
                scan = scan.resume(token);
            }
            let page = self.store.scan_range(scan)?;
            let mut last_hour = 0;
            for (key, value) in &page.rows {
                // A key under the prefix that is not an hour row is not ours to read.
                let Some(hour) = hour_of_key(key) else {
                    continue;
                };
                last_hour = hour;
                let rows =
                    decode_hour(hour, &value.value).map_err(|reason| UsageError::Corrupt {
                        key: key.clone(),
                        reason,
                    })?;
                for row in rows.into_iter().filter(|r| (from..=to).contains(&r.minute)) {
                    let kept = folded.entry(row.minute).or_insert(row);
                    *kept = fold(*kept, row);
                }
            }
            // The rows of an hour are its pack and then its instance rows, so the minutes of
            // the hours before the last one read are whole.
            let whole = folded.range(..last_hour * MINUTES_PER_HOUR).count();
            match page.resume {
                Some(next) if whole < limit.get() => token = Some(next),
                _ => break,
            }
        }
        Ok(folded.into_values().take(limit.get()).collect())
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
