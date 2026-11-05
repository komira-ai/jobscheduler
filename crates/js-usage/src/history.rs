//! The record of every ended allocation: one state-store row per allocation, read back in
//! time order and kept for the history retention window.
//!
//! Key: `usage/ended/<ended-at>/<id>`, the end in milliseconds since the Unix epoch on the
//! registry's clock, zero-padded to 20 digits so a bytewise scan is in time order, and the
//! [`AllocationId`] (until allocations have job ids, the lease id's decimal text). Value: an
//! [`EndedRow`] in the layout of the `row` module, which starts with a version byte. A field
//! is added at the end of the layout under the same version, and a reader ignores bytes after
//! the fields it knows, so a row written by a later build reads here; a row whose version this
//! build does not read is skipped by [`EndedHistory::range`], not fatal.
//!
//! The row is the first form of the allocation record of the design (section 10.1, "Usage
//! history"): today the registry writes one when a lease's booking ends (a stop, or a
//! whole-node prepare that failed), and
//! the engine that will own allocations writes the same row, under the same key, when it frees
//! one.
//!
//! The writer is an [`AllocationLog`]. The registry hands it a row after its own lock is
//! released, on the thread that ended the lease, so a log never blocks: [`crate::UsageHub`],
//! the log a server runs, adds what it measured of the lease and queues the row, and its tick
//! writes the queue through [`crate::UsageSink::allocation_ended`] where blocking is allowed.
//!
//! **One row per allocation.** Beside each row, `usage/ended-id/<id>` holds the row's key,
//! and both are written in one transaction only if both are absent. An allocation recorded
//! once is not recorded again, whatever end a later write gives it (a lease put back after a
//! restart and stopped a second time ends later than it did the first time). The prune
//! removes the two together.
//!
//! **Reading.** A row is returned only if it decodes and its key is the one its own end and
//! id give; any other row under the prefix is skipped and counted.

use std::fmt;
use std::num::NonZeroUsize;
use std::ops::Bound;

use js_state::{Cond, ResumeToken, ScanRange, Store, Txn, Versioned};
use js_types::{HostId, LeaseId};

use crate::error::UsageError;
use crate::key::ProfileKey;

mod row;

pub use row::{ENDED_ROW_VERSION, EndedRowError};
pub(crate) use row::{decode, encode};

/// The key prefix of every ended-allocation row.
pub const ENDED_PREFIX: &str = "usage/ended/";

/// The key prefix of the index of the allocations recorded: `usage/ended-id/<id>` holds the
/// key of the allocation's row.
pub const ENDED_ID_PREFIX: &str = "usage/ended-id/";

/// The longest an [`AllocationId`] may be, in bytes.
pub const MAX_ALLOCATION_ID_LEN: usize = 63;

/// Rows per page of a scan.
const PAGE: NonZeroUsize = NonZeroUsize::MIN.saturating_add(255);

/// Why a string is not an [`AllocationId`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("an allocation id is 1 to {MAX_ALLOCATION_ID_LEN} bytes of [a-z0-9._-]")]
pub struct BadAllocationId;

/// What an ended allocation is named by in its row and its key: 1 to 63 bytes of
/// `[a-z0-9._-]`, the alphabet of a job id, so it is one state-store key component.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AllocationId(String);

impl AllocationId {
    /// The id named `text`.
    ///
    /// # Errors
    /// [`BadAllocationId`] when `text` is empty, longer than [`MAX_ALLOCATION_ID_LEN`] bytes
    /// or holds a byte outside `[a-z0-9._-]`.
    pub fn parse(text: &str) -> Result<Self, BadAllocationId> {
        let fits = (1..=MAX_ALLOCATION_ID_LEN).contains(&text.len());
        let allowed = |b: u8| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-');
        match fits & text.bytes().all(allowed) {
            true => Ok(Self(text.to_owned())),
            false => Err(BadAllocationId),
        }
    }

    /// The id of the allocation a lease's booking is: the lease id's decimal text
    /// (`<term>-<sequence>`), until allocations have job ids.
    #[must_use]
    pub fn of_lease(lease: &LeaseId) -> Self {
        Self(lease.to_string())
    }

    /// The id as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AllocationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why an allocation ended, as far as the code that ended it can tell; the row stores the
/// stable [`EndedReason::name`].
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EndedReason {
    /// The allocation was given back, or ended for a cause its writer cannot tell apart.
    Released,
    /// The lease's time to live ran out.
    Expired,
    /// The host was lost (silent past the grace) when the allocation ended.
    Lost,
    /// A whole-node request was cancelled while it waited for a host: nothing was booked.
    CancelledWhileWaiting,
    /// The work never started on the host (a whole-node prepare that failed).
    FailedToStart,
    /// The allocation ended at or after the latest end its wall limit allows.
    StoppedAtWallLimit,
}

/// Every reason, in the order of the enum.
const REASONS: [EndedReason; 6] = [
    EndedReason::Released,
    EndedReason::Expired,
    EndedReason::Lost,
    EndedReason::CancelledWhileWaiting,
    EndedReason::FailedToStart,
    EndedReason::StoppedAtWallLimit,
];

impl EndedReason {
    /// The name the row stores; it never changes once released.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Released => "released",
            Self::Expired => "expired",
            Self::Lost => "lost",
            Self::CancelledWhileWaiting => "cancelled-while-waiting",
            Self::FailedToStart => "failed-to-start",
            Self::StoppedAtWallLimit => "stopped-at-wall-limit",
        }
    }

    /// The reason named `name`, or `None` for a name this build does not know.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        REASONS.into_iter().find(|reason| reason.name() == name)
    }
}

/// What became of the host a whole-node request had set aside for it (its mark), as far as
/// the allocation's writer saw it; the row stores the stable [`DrainOutcome::name`].
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DrainOutcome {
    /// Not a whole-node request: an ordinary lease is placed on arrival and drains nothing.
    None,
    /// A whole-node request that held no mark: it was placed on a host that needed no
    /// drain, or it was cancelled before any host was set aside for it.
    NoMark,
    /// A whole-node request placed on the host its mark set aside: the mark held to the end.
    MarkHeld,
    /// A whole-node request whose mark was dropped: it held a mark at some time while it
    /// waited, or gave a host up, and did not start on the host its mark set aside (it
    /// started elsewhere, or was cancelled).
    MarkDropped,
    /// A whole-node allocation put back after a restart: what its request went through was
    /// not kept.
    Unknown,
}

/// Every drain outcome, in the order of the enum.
const DRAINS: [DrainOutcome; 5] = [
    DrainOutcome::None,
    DrainOutcome::NoMark,
    DrainOutcome::MarkHeld,
    DrainOutcome::MarkDropped,
    DrainOutcome::Unknown,
];

impl DrainOutcome {
    /// The name the row stores; it never changes once released.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::NoMark => "no-mark",
            Self::MarkHeld => "mark-held",
            Self::MarkDropped => "mark-dropped",
            Self::Unknown => "unknown",
        }
    }

    /// The outcome named `name`, or `None` for a name this build does not know.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        DRAINS.into_iter().find(|drain| drain.name() == name)
    }
}

/// What an allocation booked on its host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Booked {
    /// Whether it booked the whole host, whatever the figures below add up to.
    pub whole_host: bool,
    /// The CPU it booked, in thousandths of a core.
    pub cpu_milli: u64,
    /// The memory it booked, in bytes.
    pub memory_bytes: u64,
}

/// What an allocation was measured to use, over the minutes measured: the highest minute of
/// CPU and of working set, and the CPU averaged over the minutes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Used {
    /// The highest mean CPU of a minute, in thousandths of a core.
    pub cpu_milli: u64,
    /// The highest working set of a minute, in bytes.
    pub memory_bytes: u64,
    /// The mean CPU of the minutes measured, in thousandths of a core (rounded down).
    pub cpu_mean_milli: u64,
}

/// The record of one ended allocation: the fields of the design's usage history (section
/// 10.1). Times are milliseconds on the clock of the code that wrote the row (the registry's
/// for a lease), ends since the Unix epoch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EndedRow {
    /// The allocation: until allocations have job ids, the lease id's text.
    pub id: AllocationId,
    /// The profile key a run of the allocation named, if one did: the first a run of it was
    /// queued with.
    pub profile: Option<ProfileKey>,
    /// The host it was booked on; `None` for a request cancelled before it had one.
    pub host: Option<HostId>,
    /// What it booked; `None` when it had no host.
    pub booked: Option<Booked>,
    /// How long it waited for its host: for a whole-node request, from when it was first
    /// presented to the start of its lease; zero for an ordinary lease, which is placed on
    /// arrival. `None` when it is not known (an allocation put back after a restart).
    pub queued_ms: Option<u64>,
    /// From its start to its end: zero for a request cancelled before it had a host; `None`
    /// when the start is not known (a lease put back after a restart with no start kept).
    pub run_ms: Option<u64>,
    /// Why it ended.
    pub ended_reason: EndedReason,
    /// What became of the host set aside for a whole-node request.
    pub drain_outcome: DrainOutcome,
    /// How many hosts a whole-node request gave up waiting for (drain give-ups); `None` for
    /// an ordinary lease, and when it is not known.
    pub give_up_outcome: Option<u32>,
    /// The end the estimate feed gave it, if the feed had one.
    pub estimated_end: Option<u64>,
    /// When it ended: the key's time.
    pub actual_end: u64,
    /// `actual_end` less `estimated_end`, positive for an allocation that outlived its
    /// estimate; `None` without an estimate.
    pub error_ms: Option<i64>,
    /// What it was measured to use; `None` when nothing was measured.
    pub used: Option<Used>,
}

/// The key of the row of allocation `id`, which ended at `ended_at_ms`.
#[must_use]
pub fn ended_key(ended_at_ms: u64, id: &AllocationId) -> String {
    format!("{ENDED_PREFIX}{ended_at_ms:020}/{id}")
}

/// The key of the index entry of allocation `id`.
#[must_use]
pub fn ended_id_key(id: &AllocationId) -> String {
    format!("{ENDED_ID_PREFIX}{id}")
}

/// Where the record of each ended allocation goes.
///
/// [`AllocationLog::ended`] is called on the thread that ended the allocation, after the
/// caller's own lock is released, and must neither block nor fail: a log that cannot keep a
/// row counts the loss itself.
pub trait AllocationLog: Send + Sync {
    /// Takes the record of an allocation that ended.
    fn ended(&self, row: EndedRow);
}

/// The log that keeps nothing.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoLog;

impl AllocationLog for NoLog {
    fn ended(&self, _: EndedRow) {}
}

/// The rows [`EndedHistory::range`] read, and how many it skipped.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EndedRead {
    /// The rows, oldest end first.
    pub rows: Vec<EndedRow>,
    /// Rows in the range this build does not read (a later version, or bytes that do not
    /// decode); they are left in place.
    pub skipped: u64,
}

/// The ended-allocation rows, over a state store.
pub struct EndedHistory<'a> {
    store: &'a dyn Store,
}

impl<'a> EndedHistory<'a> {
    /// The rows kept in `store`.
    #[must_use]
    pub fn new(store: &'a dyn Store) -> Self {
        Self { store }
    }

    /// Writes `row` and its index entry unless the allocation is recorded already (under
    /// any end); whether it wrote. A row written again, a retry after a write whose answer
    /// was lost or a second stop of the same allocation, changes nothing.
    ///
    /// # Errors
    /// [`UsageError::Store`] when the store fails; nothing is written.
    pub fn put(&self, row: &EndedRow) -> Result<bool, UsageError> {
        let key = ended_key(row.actual_end, &row.id);
        let index = ended_id_key(&row.id);
        let txn = Txn::new()
            .put(index, key.clone().into_bytes(), Cond::Absent)
            .put(key, encode(row), Cond::Absent);
        Ok(self.store.txn(txn)?.is_ok())
    }

    /// At most `limit` rows of the allocations that ended from `from_ms` to `to_ms`, both
    /// included, oldest end first (rows of one end in id order). A row this build does not
    /// read, or whose key is not the one its end and id give, is skipped and counted, and
    /// does not count towards `limit`.
    ///
    /// # Errors
    /// [`UsageError::Store`] when the store fails.
    pub fn range(
        &self,
        from_ms: u64,
        to_ms: u64,
        limit: NonZeroUsize,
    ) -> Result<EndedRead, UsageError> {
        let mut read = EndedRead::default();
        // A `from_ms` after `to_ms` gives bounds that cross, which scan nothing.
        let lower = format!("{ENDED_PREFIX}{from_ms:020}/");
        // Every key of an end at `to_ms` sorts before this one, since `/` sorts before `0`,
        // and every key of a later end after it.
        let upper = format!("{ENDED_PREFIX}{to_ms:020}0");
        let mut token: Option<ResumeToken> = None;
        loop {
            let mut scan = ScanRange {
                lower: Bound::Included(&lower),
                upper: Bound::Excluded(&upper),
                ..ScanRange::new(ENDED_PREFIX, PAGE)
            };
            if let Some(token) = &token {
                scan = scan.resume(token);
            }
            let page = self.store.scan_range(scan)?;
            for (key, held) in &page.rows {
                let row = decode(&held.value).ok();
                let row = row.filter(|row| ended_key(row.actual_end, &row.id) == *key);
                match row {
                    Some(row) => read.rows.push(row),
                    None => read.skipped += 1,
                }
            }
            match page.resume {
                Some(next) if read.rows.len() < limit.get() => token = Some(next),
                _ => break,
            }
        }
        read.rows.truncate(limit.get());
        Ok(read)
    }

    /// Deletes every row of an allocation that ended before `now_ms` less `retention_ms`,
    /// with its index entry; a row that ended exactly then is kept. Run it from the daily
    /// prune. How many rows it deleted; a row rewritten between the scan and its delete is
    /// left, with its index entry, for the next prune.
    ///
    /// # Errors
    /// [`UsageError::Store`] when a scan or a delete fails; rows already deleted stay so.
    pub fn prune(&self, now_ms: u64, retention_ms: u64) -> Result<u64, UsageError> {
        let cutoff = now_ms.saturating_sub(retention_ms);
        // Every key of an end before `cutoff` sorts before this one, and every key of an
        // end at `cutoff` or later after it.
        let upper = format!("{ENDED_PREFIX}{cutoff:020}/");
        let mut deleted = 0;
        let mut token: Option<ResumeToken> = None;
        loop {
            let mut scan = ScanRange {
                upper: Bound::Excluded(&upper),
                ..ScanRange::new(ENDED_PREFIX, PAGE)
            };
            if let Some(token) = &token {
                scan = scan.resume(token);
            }
            let page = self.store.scan_range(scan)?;
            // One transaction per row: each is judged on its own version. The id is the
            // last component of the row's key.
            let delete = |(key, held): (String, Versioned)| {
                let id = key.rsplit('/').next().unwrap_or_default();
                let index = format!("{ENDED_ID_PREFIX}{id}");
                Txn::new()
                    .delete(index, Cond::Any)
                    .delete(key, Cond::Version(held.version))
            };
            let txns: Vec<Txn> = page.rows.into_iter().map(delete).collect();
            if !txns.is_empty() {
                let done = self.store.apply_batch(txns)?;
                deleted += done.iter().filter(|result| result.is_ok()).count() as u64;
            }
            match page.resume {
                Some(next) => token = Some(next),
                None => return Ok(deleted),
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests;
