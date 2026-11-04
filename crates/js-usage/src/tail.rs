//! The tail counters' durable home: the seam the registry feeds ([`TailSink`]), the plain
//! values that cross it ([`TailTotals`], [`TailDelta`]), and the store implementation
//! ([`StoreTailSink`]).
//!
//! The registry counts what the tail rule is measured by: leases started on a host a
//! whole-node request marked, how many of them were backfill, and how far each ended
//! lease's end was from its estimate, as a count, a signed sum and signed buckets. It
//! keeps the counts in memory and hands each change to a sink as a [`TailDelta`]; at
//! start it asks the sink for what is stored ([`TailSink::load`]) and counts on from it.
//! The trait lives here, not in the registry, because the registry already depends on
//! this crate and this crate on no registry.
//!
//! # Storage
//!
//! Key space [`TAIL_PREFIX`]: one row per counter, named by its field
//! (`usage/tail/started_on_marked`, `usage/tail/backfilled`, `usage/tail/ended_estimated`,
//! `usage/tail/ended_unestimated`, `usage/tail/error_sum_ms` and `usage/tail/error_bucket_0`
//! to `usage/tail/error_bucket_7`), each a version byte ([`TAIL_ROW_VERSION`]) and the value
//! as 8 little-endian bytes (unsigned for a count, two's complement for the sum). The row
//! `usage/tail/error_edges_ms` holds the bucket edges the counts were made with: a version
//! byte, the number of edges and each edge as 8 little-endian bytes.
//!
//! A write adds a delta to the stored values: it reads the rows, and writes the new values
//! in one transaction conditional on the version of every row it read (absent for a row
//! not yet written) and of the edges row. A transaction that meets a changed row is read
//! and tried again, up to [`TAIL_WRITE_TRIES`] times. So two writers, or two boots one
//! after the other, each add their own counts: neither loses nor doubles the other's. The
//! server never resets the rows; only a person deleting them does.
//!
//! A stored set made with other edges is refused ([`TailSinkError::OtherEdges`]): its
//! buckets mean other ranges, so they are neither read nor added to, and the rows are
//! left as they are. So is one whose edges row does not read
//! ([`TailSinkError::Unreadable`]) and counter rows with no edges row
//! ([`TailSinkError::NoEdges`]). A counter row of a newer version (a later build that stores a field
//! another way) is skipped, read as zero and counted ([`TailSinkCounters::rows_skipped`]),
//! and never overwritten.

use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

use js_state::{Cond, Store, StoreError, Txn, Versioned};

/// The state-store prefix of the tail counter rows.
pub const TAIL_PREFIX: &str = "usage/tail/";

/// How many estimate-error buckets there are: one more than there are edges.
pub const TAIL_BUCKETS: usize = 8;

/// The version byte of a tail counter row this build writes and reads.
pub const TAIL_ROW_VERSION: u8 = 1;

/// How many times a write is read and tried again when a row changed under it.
pub const TAIL_WRITE_TRIES: usize = 8;

/// The name, under [`TAIL_PREFIX`], of the row of the bucket edges.
const EDGES: &str = "error_edges_ms";

/// The counters before the buckets: four counts, then the sum.
const SCALARS: [&str; 5] = [
    "started_on_marked",
    "backfilled",
    "ended_estimated",
    "ended_unestimated",
    "error_sum_ms",
];

/// The place of the sum among the counters; every other counter is a count.
const SUM: usize = 4;

/// How many counters, so how many counter rows.
const COUNTERS: usize = SCALARS.len() + TAIL_BUCKETS;

/// The bytes of a counter row: the version and the value.
const ROW_LEN: usize = 9;

/// Every counter's value, widened so the sum and the counts add alike.
type Values = [i128; COUNTERS];

/// The name of counter `i` under [`TAIL_PREFIX`].
fn name_of(i: usize) -> String {
    match SCALARS.get(i) {
        Some(name) => (*name).to_owned(),
        None => format!("error_bucket_{}", i - SCALARS.len()),
    }
}

/// The counter a row name under [`TAIL_PREFIX`] holds, or `None` for a name this build
/// does not know (a later build's field).
fn index_of(name: &str) -> Option<usize> {
    (0..COUNTERS).find(|i| name_of(*i) == name)
}

/// `value` brought into the range of counter `i`: a count is unsigned, the sum signed.
fn clamp(i: usize, value: i128) -> i128 {
    match i == SUM {
        true => value.clamp(i128::from(i64::MIN), i128::from(i64::MAX)),
        false => value.clamp(0, i128::from(u64::MAX)),
    }
}

macro_rules! tail_values {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
        pub struct $name {
            /// Leases started on a host that carried a mark, the marked request's own
            /// included.
            pub started_on_marked: u64,
            /// Of those, the leases that were not the marked request's own: backfill.
            pub backfilled: u64,
            /// Leases that ended with an estimate.
            pub ended_estimated: u64,
            /// Leases that ended with no estimate.
            pub ended_unestimated: u64,
            /// The sum of the errors, actual end minus estimated end, in milliseconds.
            pub error_sum_ms: i64,
            /// The errors by bucket: bucket `i` counts the errors up to and including edge
            /// `i` and above the edge before it; the last counts those above the last edge.
            pub error_buckets: [u64; TAIL_BUCKETS],
        }

        impl $name {
            fn values(&self) -> Values {
                let mut values = [0; COUNTERS];
                values[0] = i128::from(self.started_on_marked);
                values[1] = i128::from(self.backfilled);
                values[2] = i128::from(self.ended_estimated);
                values[3] = i128::from(self.ended_unestimated);
                values[SUM] = i128::from(self.error_sum_ms);
                for (slot, bucket) in values[SCALARS.len()..].iter_mut().zip(self.error_buckets) {
                    *slot = i128::from(bucket);
                }
                values
            }

            fn from_values(values: Values) -> Self {
                // Every value is clamped into its counter's range first.
                let count = |i: usize| u64::try_from(clamp(i, values[i])).unwrap_or_default();
                let sum = i64::try_from(clamp(SUM, values[SUM])).unwrap_or_default();
                let mut error_buckets = [0; TAIL_BUCKETS];
                for (b, bucket) in error_buckets.iter_mut().enumerate() {
                    *bucket = count(SCALARS.len() + b);
                }
                Self {
                    started_on_marked: count(0),
                    backfilled: count(1),
                    ended_estimated: count(2),
                    ended_unestimated: count(3),
                    error_sum_ms: sum,
                    error_buckets,
                }
            }

            fn plus(self, delta: &TailDelta) -> Self {
                let (mut values, more) = (self.values(), delta.values());
                for (i, value) in values.iter_mut().enumerate() {
                    *value = clamp(i, *value + more[i]);
                }
                Self::from_values(values)
            }
        }
    };
}

tail_values! {
    /// The tail counters as a whole: what is stored, or what the registry has counted.
    /// Every count saturates at its largest value and the sum at either end of its range.
    /// Within those ranges the totals are exact whatever the order and grouping of the
    /// changes; only a total that reaches a limit may depend on them.
    TailTotals
}

tail_values! {
    /// A change to the tail counters, added to what is stored: the counts of one start or
    /// one end, or of several merged.
    TailDelta
}

impl TailTotals {
    /// Adds `delta` to the totals, each counter saturating.
    pub fn add(&mut self, delta: &TailDelta) {
        *self = self.plus(delta);
    }
}

impl TailDelta {
    /// Whether the delta changes nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Merges `other` into this delta, each counter saturating.
    pub fn merge(&mut self, other: &TailDelta) {
        *self = self.plus(other);
    }
}

/// Why a stored tail row does not read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TailRowError {
    /// The row has no version byte.
    #[error("the row is empty")]
    Empty,
    /// A version byte this build does not read.
    #[error("the row has version {0}, which this build does not read")]
    Version(u8),
    /// A length that does not match the row's version.
    #[error("the row's {0} bytes do not match its version")]
    Length(usize),
}

/// Why the stored tail counters could not be read or added to.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TailSinkError {
    /// The state store failed.
    #[error("tail counters: {0}")]
    Store(#[from] StoreError),
    /// The stored counters were bucketed by other edges, so their buckets are other
    /// ranges. They are left as they are; the counters start empty beside them.
    #[error("the stored tail counters use bucket edges {stored:?}, this build {build:?}")]
    OtherEdges {
        /// The edges stored with the counters.
        stored: Vec<i64>,
        /// The edges this build counts by.
        build: Vec<i64>,
    },
    /// The row of the edges does not read, so the counters cannot be checked against
    /// this build's edges. It is left as it is.
    #[error("tail counter row {key} does not read: {reason}")]
    Unreadable {
        /// The row's key.
        key: String,
        /// What is wrong with it.
        reason: TailRowError,
    },
    /// Counter rows are stored with no row of the edges beside them, so the ranges their
    /// buckets count are unknown. They are left as they are.
    #[error("tail counter rows are stored with no edges row; their buckets cannot be read")]
    NoEdges,
    /// The rows changed under every try of a write: other writers kept writing.
    #[error("tail counter rows changed under each of {tries} tries of a write")]
    Conflict {
        /// How many tries were made.
        tries: usize,
    },
}

/// What a tail sink has counted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TailSinkCounters {
    /// Writes that did not reach the store: the counts they carried are in memory only.
    pub writes_failed: u64,
    /// Counter rows passed over because they do not read as this build's rows (a newer
    /// version, or a broken one), at a load or a write.
    pub rows_skipped: u64,
}

/// Where the registry keeps its tail counters beyond its own memory.
///
/// The registry calls [`TailSink::add`] after it has released its lock, so an
/// implementation may take its own locks; it never calls the store under the registry's.
pub trait TailSink: Send + Sync {
    /// The counters stored so far, which the registry counts on from.
    ///
    /// # Errors
    /// [`TailSinkError`] when they cannot be read, or were made with other bucket edges;
    /// the registry then starts from zero.
    fn load(&self) -> Result<TailTotals, TailSinkError>;

    /// Hands the sink `delta` to add to the stored counters, at once or at its next
    /// write. It never fails and never blocks on a store: a write that does not land costs
    /// the stored counters only, and the sink counts it.
    fn add(&self, delta: &TailDelta);

    /// What the sink has counted.
    fn counters(&self) -> TailSinkCounters {
        TailSinkCounters::default()
    }
}

/// The sink that keeps nothing: it loads zero and forgets every delta.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoTail;

impl TailSink for NoTail {
    fn load(&self) -> Result<TailTotals, TailSinkError> {
        Ok(TailTotals::default())
    }

    fn add(&self, _: &TailDelta) {}
}

/// The key of the row named `name`.
fn key_of(name: &str) -> String {
    format!("{TAIL_PREFIX}{name}")
}

/// The stored form of the bucket edges.
fn encode_edges(edges: &[i64]) -> Vec<u8> {
    let mut out = vec![TAIL_ROW_VERSION];
    // There are TAIL_BUCKETS - 1 edges, far fewer than a byte counts.
    out.push(edges.len() as u8);
    edges
        .iter()
        .for_each(|edge| out.extend_from_slice(&edge.to_le_bytes()));
    out
}

/// The version byte of a row, and the bytes after it.
fn versioned(bytes: &[u8]) -> Result<&[u8], TailRowError> {
    let (&version, rest) = bytes.split_first().ok_or(TailRowError::Empty)?;
    match version == TAIL_ROW_VERSION {
        true => Ok(rest),
        false => Err(TailRowError::Version(version)),
    }
}

/// The bucket edges a stored edges row holds.
fn decode_edges(bytes: &[u8]) -> Result<Vec<i64>, TailRowError> {
    let rest = versioned(bytes)?;
    let (&count, rest) = rest
        .split_first()
        .ok_or(TailRowError::Length(bytes.len()))?;
    if rest.len() != usize::from(count) * 8 {
        return Err(TailRowError::Length(bytes.len()));
    }
    let edges = rest.chunks_exact(8).map(le_bytes).map(i64::from_le_bytes);
    Ok(edges.collect())
}

/// Eight bytes of a chunk that `chunks_exact(8)` made.
fn le_bytes(chunk: &[u8]) -> [u8; 8] {
    let mut out = [0; 8];
    out.copy_from_slice(chunk);
    out
}

/// The stored form of counter `i` at `value`.
fn encode_value(i: usize, value: i128) -> Vec<u8> {
    let value = clamp(i, value);
    let bytes = match i == SUM {
        true => i64::try_from(value).unwrap_or_default().to_le_bytes(),
        false => u64::try_from(value).unwrap_or_default().to_le_bytes(),
    };
    let mut out = vec![TAIL_ROW_VERSION];
    out.extend_from_slice(&bytes);
    out
}

/// The value of counter `i` a stored row holds.
fn decode_value(i: usize, bytes: &[u8]) -> Result<i128, TailRowError> {
    let rest = versioned(bytes)?;
    if bytes.len() != ROW_LEN {
        return Err(TailRowError::Length(bytes.len()));
    }
    let raw = le_bytes(rest);
    Ok(match i == SUM {
        true => i128::from(i64::from_le_bytes(raw)),
        false => i128::from(u64::from_le_bytes(raw)),
    })
}

/// The tail rows as one scan found them.
struct Stored {
    /// The edges row, if there is one.
    edges: Option<Versioned>,
    /// Each counter's row, if it has one.
    rows: Vec<Option<Versioned>>,
}

/// The tail counters in a state store, under [`TAIL_PREFIX`].
///
/// [`StoreTailSink::add`] only merges the delta into the counts waiting to be written, so
/// it never waits on the store; [`StoreTailSink::flush`] writes them, and the server calls
/// it where it writes the rest of usage, on the pool for blocking calls.
pub struct StoreTailSink {
    store: Arc<dyn Store>,
    edges: [i64; TAIL_BUCKETS - 1],
    state: Mutex<State>,
    /// Held for the whole of a flush, so flushes run one after another: a flush that
    /// starts while another writes waits for it, and so sees the counts a failed write
    /// put back. [`StoreTailSink::add`] never takes it.
    flushing: Gate,
}

/// What a flush did that did not fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TailFlush {
    /// Nothing waited, and the store was not asked.
    Idle,
    /// The counts that waited are in the store.
    Written,
}

#[derive(Default)]
struct State {
    pending: TailDelta,
    counters: TailSinkCounters,
}

impl StoreTailSink {
    /// The counters kept in `store`, bucketed by `edges` (the registry's
    /// `ERROR_EDGES_MS`).
    #[must_use]
    pub fn new(store: Arc<dyn Store>, edges: [i64; TAIL_BUCKETS - 1]) -> Self {
        Self {
            store,
            edges,
            state: Mutex::new(State::default()),
            flushing: Gate::default(),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The counts added and not yet written.
    #[must_use]
    pub fn pending(&self) -> TailDelta {
        self.state().pending
    }

    /// Writes the counts waiting to be written, after any flush already under way has
    /// ended. On a store failure or a conflict they keep waiting for the next flush, with
    /// whatever was added meanwhile, and the failure is counted
    /// ([`TailSinkCounters::writes_failed`]); on a refusal (other edges, an edges row that
    /// does not read, counter rows with no edges row) they are dropped and counted, since
    /// they can never be written beside those rows. With nothing waiting the store is not
    /// asked ([`TailFlush::Idle`]).
    ///
    /// # Errors
    /// [`TailSinkError`] when the write did not land.
    pub fn flush(&self) -> Result<TailFlush, TailSinkError> {
        let _flushing = self.flushing.enter();
        let delta = std::mem::take(&mut self.state().pending);
        if delta.is_empty() {
            return Ok(TailFlush::Idle);
        }
        let written = self.write(&delta);
        let mut state = self.state();
        if written.is_err() {
            state.counters.writes_failed += 1;
        }
        if let Err(TailSinkError::Store(_) | TailSinkError::Conflict { .. }) = &written {
            state.pending.merge(&delta);
        }
        written.map(|()| TailFlush::Written)
    }

    /// The tail rows in the store, one scan.
    fn scan(&self) -> Result<Stored, TailSinkError> {
        let mut stored = Stored {
            edges: None,
            rows: vec![None; COUNTERS],
        };
        for (key, held) in self.store.scan_prefix(TAIL_PREFIX)? {
            let name = &key[TAIL_PREFIX.len()..];
            match (name == EDGES, index_of(name)) {
                (true, _) => stored.edges = Some(held),
                (false, Some(i)) => stored.rows[i] = Some(held),
                // A row of a field this build does not know is a later build's.
                (false, None) => {}
            }
        }
        Ok(stored)
    }

    /// Refuses a stored set this build cannot add to: one whose edges are not this
    /// build's, or do not read, or counter rows with no edges row beside them.
    fn check_set(&self, stored: &Stored) -> Result<(), TailSinkError> {
        match &stored.edges {
            Some(edges) => self.check_edges(edges),
            None => match stored.rows.iter().any(Option::is_some) {
                true => Err(TailSinkError::NoEdges),
                false => Ok(()),
            },
        }
    }

    /// Refuses a stored set whose edges are not this build's.
    fn check_edges(&self, edges: &Versioned) -> Result<(), TailSinkError> {
        let stored = decode_edges(&edges.value).map_err(|reason| TailSinkError::Unreadable {
            key: key_of(EDGES),
            reason,
        })?;
        match stored == self.edges {
            true => Ok(()),
            false => Err(TailSinkError::OtherEdges {
                stored,
                build: self.edges.to_vec(),
            }),
        }
    }

    fn skipped(&self) {
        self.state().counters.rows_skipped += 1;
    }

    /// Adds `delta` to the stored rows: one transaction conditional on every row read,
    /// read and tried again while another writer got in first.
    fn write(&self, delta: &TailDelta) -> Result<(), TailSinkError> {
        let more = delta.values();
        for _ in 0..TAIL_WRITE_TRIES {
            let stored = self.scan()?;
            self.check_set(&stored)?;
            let edges_key = key_of(EDGES);
            let mut txn = match &stored.edges {
                Some(edges) => Txn::new().check(edges_key, Cond::Version(edges.version)),
                None => Txn::new().put(edges_key, encode_edges(&self.edges), Cond::Absent),
            };
            for (i, row) in stored.rows.iter().enumerate() {
                if more[i] == 0 {
                    continue;
                }
                let (old, cond) = match row {
                    Some(row) => match decode_value(i, &row.value) {
                        Ok(old) => (old, Cond::Version(row.version)),
                        // Never overwrite a row this build does not read.
                        Err(_) => {
                            self.skipped();
                            continue;
                        }
                    },
                    None => (0, Cond::Absent),
                };
                txn = txn.put(key_of(&name_of(i)), encode_value(i, old + more[i]), cond);
            }
            if self.store.txn(txn)?.is_ok() {
                return Ok(());
            }
        }
        Err(TailSinkError::Conflict {
            tries: TAIL_WRITE_TRIES,
        })
    }
}

/// One flush at a time: [`Gate::enter`] waits while another flush holds the gate.
#[derive(Default)]
struct Gate {
    state: Mutex<GateState>,
    left: Condvar,
}

#[derive(Default)]
struct GateState {
    /// Whether a flush holds the gate.
    held: bool,
    /// How many flushes wait for it.
    waiting: usize,
}

impl Gate {
    fn state(&self) -> MutexGuard<'_, GateState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Holds the gate until the pass is dropped, after waiting for any flush that holds it.
    fn enter(&self) -> Pass<'_> {
        let mut state = self.state();
        while state.held {
            state.waiting += 1;
            // Whoever waits for the count of waiters to change (a test) is told.
            self.left.notify_all();
            state = self
                .left
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
            state.waiting -= 1;
        }
        state.held = true;
        Pass(self)
    }
}

/// The hold of a [`Gate`], let go when dropped.
struct Pass<'a>(&'a Gate);

impl Drop for Pass<'_> {
    fn drop(&mut self) {
        self.0.state().held = false;
        self.0.left.notify_one();
    }
}

impl TailSink for StoreTailSink {
    fn load(&self) -> Result<TailTotals, TailSinkError> {
        let stored = self.scan()?;
        self.check_set(&stored)?;
        let mut values = [0; COUNTERS];
        for (i, row) in stored.rows.iter().enumerate() {
            let Some(row) = row else {
                continue;
            };
            match decode_value(i, &row.value) {
                Ok(value) => values[i] = value,
                Err(_) => self.skipped(),
            }
        }
        Ok(TailTotals::from_values(values))
    }

    fn add(&self, delta: &TailDelta) {
        self.state().pending.merge(delta);
    }

    fn counters(&self) -> TailSinkCounters {
        self.state().counters
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::{self, TryRecvError};
    use std::thread;

    use super::*;
    use crate::testkit::TestStore;

    /// Asserts that a flush that starts while another holds the flush gate waits on the
    /// gate, and then writes what the other put back. The test holds the gate itself, as a
    /// flush whose write is under way does, and holds the gate's lock from before the second
    /// flush starts, so the second flush can neither count itself waiting nor finish unseen:
    /// the test then waits on the gate's condition for one of two events, the second flush
    /// counted among the waiters, or the second flush returned (its thread takes the lock
    /// before it says so). With the gate the first always comes; without it the second
    /// always does, and the test fails, whatever the schedule. Catches: flushes that
    /// overlap, so a final flush returns with nothing written while an earlier one still
    /// holds counts it then puts back.
    #[test]
    fn a_second_flush_waits_on_the_gate_and_writes_what_the_first_put_back() {
        let store = TestStore::new();
        let sink = Arc::new(StoreTailSink::new(store, [0; TAIL_BUCKETS - 1]));
        let held = sink.flushing.enter();
        let mut gate = sink.flushing.state();
        let (done, finished) = mpsc::channel();
        let second = Arc::clone(&sink);
        let second = thread::spawn(move || {
            let flushed = second.flush();
            let _told = second.flushing.state();
            let _ = done.send(());
            second.flushing.left.notify_all();
            flushed
        });
        while gate.waiting == 0 {
            let ran_past = finished.try_recv() != Err(TryRecvError::Empty);
            assert!(!ran_past, "the second flush ran past the gate");
            gate = sink
                .flushing
                .left
                .wait(gate)
                .unwrap_or_else(PoisonError::into_inner);
        }
        drop(gate);
        let back = TailDelta {
            backfilled: 1,
            ..TailDelta::default()
        };
        sink.state().pending.merge(&back);
        drop(held);
        assert_eq!(second.join().expect("no panic"), Ok(TailFlush::Written));
        assert_eq!(sink.pending(), TailDelta::default());
        assert_eq!(sink.load().map(|t| t.backfilled), Ok(1));
        assert_eq!(sink.flushing.state().waiting, 0);
    }
}
