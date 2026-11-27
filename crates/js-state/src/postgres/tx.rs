//! One database transaction of the store: locks the keys a [`Txn`] names, judges its
//! preconditions against what it locked, and keeps its writes until its tail, which takes
//! their revisions from the commit head and writes the rows, feed rows and head together.
//!
//! The transaction runs read committed. Its body locks every key it reads or writes, in key
//! order within one statement: a key with a row is locked (`FOR UPDATE` semantics, through
//! `INSERT ... ON CONFLICT DO UPDATE ... WHERE false`, which locks the row it conflicts with
//! and changes nothing), and a key with none is reserved by the same statement with an
//! uncommitted placeholder row, which no other transaction sees and which makes any other
//! writer of the key wait on the key's unique index until this one ends. A locked row is the
//! latest committed one and stays so to the end, and a reserved key stays absent: the
//! preconditions judged on them hold at the commit.
//!
//! The commit head is one row. Only the tail takes it: `LOCK TABLE head` (the mode every
//! release takes, so writers of an older release queue the same way), then the head moves by
//! the number of writes, then one statement writes the feed rows, the rows and the deletes
//! (placeholders included), then the commit. The head is held for those statements and the
//! commit's flush and for nothing else, so revisions commit in the order they were taken and
//! the feed has no gap a poller could step over (C5), while the bodies of any number of
//! transactions run at once. A tail locks no key (its body holds them all), so it waits for
//! nothing but the head.
//!
//! Inside the transaction a written row's version is a provisional one, below zero, that no
//! precondition can name; the revisions a transaction reports are settled by its tail
//! ([`Revs`]).

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use postgres::Error;

use super::session::{Pooled, Session};
use super::{Fail, Flaw};
use crate::{Commit, Cond, Conflict, Op, Revision, Txn};

const KIND_PUT: i16 = 0;
const KIND_DELETE: i16 = 1;
const KIND_EXPIRE: i16 = 2;

/// A stored row, as loaded.
#[derive(Clone, Debug)]
pub(super) struct Row {
    pub value: Vec<u8>,
    pub version: i64,
    pub expires_us: Option<i64>,
}

impl Row {
    pub(super) fn from_row(row: &postgres::Row, first: usize) -> Row {
        Row {
            value: row.get(first),
            version: row.get(first + 1),
            expires_us: row.get(first + 2),
        }
    }

    fn live(&self, now: i64) -> bool {
        self.expires_us.is_none_or(|at| at > now)
    }
}

/// A duration in whole microseconds, saturating.
pub(super) fn micros(d: Duration) -> i64 {
    i64::try_from(d.as_micros()).unwrap_or(i64::MAX)
}

/// One write the tail makes: a feed row, in the order the transaction made them.
struct Write {
    key: String,
    kind: i16,
    value: Option<Vec<u8>>,
}

/// The revisions a tail took. A transaction reports provisional revisions: the number of
/// writes made before its result was decided; [`Revs::settle`] turns one into the revision
/// of that write, or, for none, into the head the transaction committed on.
pub(super) struct Revs {
    /// The head below this transaction's writes (0 under the sequence flaw, which has none).
    base: i64,
    /// The revision of each write, in order.
    taken: Vec<i64>,
}

impl Revs {
    /// `commit` with its provisional revision replaced by the real one.
    pub(super) fn settle(&self, commit: Commit) -> Commit {
        let at = usize::try_from(commit.revision.0).unwrap_or(usize::MAX);
        let rev = at
            .checked_sub(1)
            .and_then(|i| self.taken.get(i).copied())
            .unwrap_or(self.base);
        Commit {
            revision: Revision(u64::try_from(rev).unwrap_or(0)),
            ..commit
        }
    }
}

pub(super) struct Tx<'p> {
    session: Pooled<'p>,
    /// The injected clock reading, or `None` for the database's clock.
    over: Option<i64>,
    /// The store's time: read when the transaction opens, and again after every statement
    /// that may have waited for a lock, never going back.
    now: i64,
    flaw: Flaw,
    /// Every key this transaction has locked or written, as it stands now. A key's row is
    /// locked, or reserved, so it stays what was loaded.
    cache: BTreeMap<String, Option<Row>>,
    /// The keys this transaction reserved with a placeholder row.
    reserved: BTreeSet<String>,
    /// The writes to make at the tail, in order.
    writes: Vec<Write>,
    /// Something besides the writes must be made durable: a counter moved, or feed rows were
    /// trimmed.
    kept: bool,
    open: bool,
}

impl<'p> Tx<'p> {
    /// Opens a transaction. `over` is an injected clock reading, or `None` for the
    /// database's own clock.
    pub(super) fn begin(
        mut session: Pooled<'p>,
        over: Option<i64>,
        flaw: Flaw,
    ) -> Result<Tx<'p>, Fail> {
        let begin = session
            .client
            .batch_execute("BEGIN ISOLATION LEVEL READ COMMITTED");
        // The session is judged here, as no transaction exists to roll back on drop: one that
        // met a closed connection must not go back to the pool.
        begin.inspect_err(|err| session.note_failure(err))?;
        let mut tx = Tx {
            session,
            over,
            now: 0,
            flaw,
            cache: BTreeMap::new(),
            reserved: BTreeSet::new(),
            writes: Vec::new(),
            kept: false,
            open: true,
        };
        let s: &mut Session = &mut tx.session;
        tx.now = s.client.query_one(&s.st.now, &[&over])?.get(0);
        Ok(tx)
    }

    /// The store's time as the transaction read it last.
    pub(super) fn now(&self) -> i64 {
        self.now
    }

    /// `txn` alone, its body and its tail: its keys locked, its preconditions judged and its
    /// writes made, with the revisions settled.
    pub(super) fn apply_alone(&mut self, txn: &Txn) -> Result<Result<Commit, Conflict>, Error> {
        let result = self.apply(txn)?;
        let revs = self.flush()?;
        Ok(result.map(|commit| revs.settle(commit)))
    }

    /// Takes the commit head now, before anything is read, and reads the clock again after
    /// the wait: until this transaction ends no other transaction's writes commit, so what it
    /// reads without a lock ([`Tx::peek`], [`Tx::read_prefix`]) is what its commit finds.
    pub(super) fn hold_head(&mut self) -> Result<(), Error> {
        let s: &mut Session = &mut self.session;
        s.client
            .batch_execute("LOCK TABLE head IN SHARE UPDATE EXCLUSIVE MODE")?;
        self.now = self
            .now
            .max(s.client.query_one(&s.st.now, &[&self.over])?.get(0));
        Ok(())
    }

    /// Whether the commit makes anything durable.
    pub(super) fn dirty(&self) -> bool {
        self.kept || !self.writes.is_empty()
    }

    /// The tail: takes the revisions of the writes, under the commit head, and makes the
    /// writes, the deletes and the placeholders' removal in one statement. A transaction
    /// that wrote nothing takes no lock and reads the committed head; one that keeps
    /// nothing ([`Tx::dirty`] is false) writes nothing, as its commit rolls back.
    pub(super) fn flush(&mut self) -> Result<Revs, Error> {
        let count = i64::try_from(self.writes.len()).unwrap_or(i64::MAX);
        let s: &mut Session = &mut self.session;
        let revs = match (self.flaw, count) {
            (Flaw::SequenceRevisions, _) => Revs {
                base: 0,
                taken: s
                    .client
                    .query(&s.st.seq, &[&count])?
                    .iter()
                    .map(|row| row.get(0))
                    .collect(),
            },
            (_, 0) => Revs {
                base: s.client.query_one(&s.st.head_read, &[])?.get(0),
                taken: Vec::new(),
            },
            _ => {
                s.client
                    .batch_execute("LOCK TABLE head IN SHARE UPDATE EXCLUSIVE MODE")?;
                let top: i64 = s.client.query_one(&s.st.head_move, &[&count])?.get(0);
                let base = top - count;
                Revs {
                    base,
                    taken: (base + 1..=top).collect(),
                }
            }
        };
        if self.dirty() {
            self.write(&revs)?;
        }
        Ok(revs)
    }

    /// The tail's one write statement: the feed rows at their revisions, each key's row as
    /// the transaction leaves it (at the revision of its last write), and the deletes of the
    /// keys it leaves absent that hold a row or a placeholder.
    fn write(&mut self, revs: &Revs) -> Result<(), Error> {
        let mut last: BTreeMap<&str, i64> = BTreeMap::new();
        let mut feed = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for (write, rev) in self.writes.iter().zip(&revs.taken) {
            last.insert(write.key.as_str(), *rev);
            feed.0.push(*rev);
            feed.1.push(write.key.as_str());
            feed.2.push(write.kind);
            feed.3.push(write.value.as_deref());
        }
        let mut put = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        let mut gone = Vec::new();
        let touched = last
            .keys()
            .copied()
            .chain(self.reserved.iter().map(String::as_str));
        for key in touched.collect::<BTreeSet<_>>() {
            // A key left with a row was written (a reserved key never written holds only its
            // placeholder, and is absent).
            if let (Some(Some(row)), Some(rev)) = (self.cache.get(key), last.get(key)) {
                put.0.push(key);
                put.1.push(row.value.as_slice());
                put.2.push(*rev);
                put.3.push(row.expires_us);
            } else {
                gone.push(key);
            }
        }
        let s: &mut Session = &mut self.session;
        s.client.execute(
            &s.st.write,
            &[
                &feed.0, &feed.1, &feed.2, &feed.3, &put.0, &put.1, &put.2, &put.3, &gone,
            ],
        )?;
        Ok(())
    }

    /// Makes the transaction durable, or, when it keeps nothing, rolls it back: its
    /// placeholders go with it.
    pub(super) fn commit(mut self) -> Result<(), Fail> {
        self.open = false;
        let end = if self.dirty() { "COMMIT" } else { "ROLLBACK" };
        if let Err(err) = self.session.client.batch_execute(end) {
            // A refusal from the server leaves the session usable. A lost connection,
            // including the server's FATAL as it ends the backend, leaves it unknown whether
            // the commit happened, and the session is not reused.
            self.session.note_failure(&err);
            return Err(err.into());
        }
        Ok(())
    }

    /// Adds `row`, which the transaction has locked, to what it knows (a claim locked it).
    pub(super) fn remember(&mut self, key: String, row: Row) {
        self.cache.insert(key, Some(row));
    }

    /// Locks or reserves the keys of `keys` that are not known yet, in key order in one
    /// statement, loads their rows, and expires those the clock has passed.
    pub(super) fn load(&mut self, keys: &[&str]) -> Result<(), Error> {
        let wanted: Vec<&str> = keys
            .iter()
            .copied()
            .filter(|key| !self.cache.contains_key(*key))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if !wanted.is_empty() {
            let s: &mut Session = &mut self.session;
            let reserved: BTreeSet<String> = s
                .client
                .query(&s.st.reserve, &[&wanted])?
                .iter()
                .map(|row| row.get(0))
                .collect();
            // Every key now has a row this transaction sees: its locked row or its own
            // placeholder. A key it reserved holds no row, so when it reserved them all (a
            // transaction that creates keys, as a submit's does) there is nothing to load and
            // nothing for the clock to judge. Otherwise the rows are loaded, and the clock is
            // read again, after any wait for those locks.
            let all = reserved.len() == wanted.len();
            let rows = match all {
                true => Vec::new(),
                false => s.client.query(&s.st.load, &[&wanted, &self.over])?,
            };
            for key in &wanted {
                self.cache.insert((*key).to_owned(), None);
            }
            for row in rows {
                let key: String = row.get(0);
                self.now = self.now.max(row.get(4));
                let loaded = (!reserved.contains(&key)).then(|| Row::from_row(&row, 1));
                self.cache.insert(key, loaded);
            }
            self.reserved.extend(reserved);
        }
        for key in keys {
            let dead = self
                .cache
                .get(*key)
                .and_then(Option::as_ref)
                .is_some_and(|row| !row.live(self.now));
            if dead {
                self.expire(key);
            }
        }
        Ok(())
    }

    /// Records a write and returns its provisional revision (how many writes there are).
    fn record(&mut self, key: &str, kind: i16, value: Option<Vec<u8>>) -> i64 {
        self.writes.push(Write {
            key: key.to_owned(),
            kind,
            value,
        });
        i64::try_from(self.writes.len()).unwrap_or(i64::MAX)
    }

    fn expire(&mut self, key: &str) {
        self.record(key, KIND_EXPIRE, None);
        self.cache.insert(key.to_owned(), None);
    }

    /// Removes every key the clock has expired, one `Expire` row each, in key order.
    pub(super) fn reap(&mut self) -> Result<usize, Error> {
        let now = self.now;
        let s: &mut Session = &mut self.session;
        let keys: Vec<String> = s
            .client
            .query(&s.st.due, &[&now])?
            .iter()
            .map(|row| row.get(0))
            .collect();
        for key in &keys {
            self.expire(key);
        }
        Ok(keys.len())
    }

    /// Deletes the feed rows at or below `before` or the committed head, whichever is
    /// lower; how many. Every revision at or below the committed head has committed, and
    /// none above it is touched. (Under the sequence flaw nothing moves the head, so a trim
    /// deletes nothing.)
    pub(super) fn trim(&mut self, before: i64) -> Result<u64, Error> {
        let s: &mut Session = &mut self.session;
        let removed = s.client.execute(&s.st.trim, &[&before])?;
        self.kept |= removed > 0;
        Ok(removed)
    }

    /// Locks up to `limit` live rows under `prefix`, in key order. With `skip_locked`
    /// a row another transaction holds is passed over; without it the call waits.
    pub(super) fn claim_rows(
        &mut self,
        prefix: &str,
        limit: i64,
        skip_locked: bool,
    ) -> Result<Vec<(String, Row)>, Error> {
        let now = self.now;
        let s: &mut Session = &mut self.session;
        let statement = if skip_locked {
            &s.st.claim
        } else {
            &s.st.claim_waiting
        };
        let rows = s.client.query(statement, &[&prefix, &now, &limit])?;
        Ok(rows
            .iter()
            .map(|row| (row.get(0), Row::from_row(row, 1)))
            .collect())
    }

    /// Locks the live rows of `keys`, skipping rows another transaction holds, and keeps them
    /// as the transaction's own. The rows it got, in key order.
    pub(super) fn lock_keys(&mut self, keys: &[String]) -> Result<Vec<(String, Row)>, Error> {
        let now = self.now;
        let s: &mut Session = &mut self.session;
        let rows = s.client.query(&s.st.claim_keys, &[&keys, &now])?;
        let rows: Vec<(String, Row)> = rows
            .iter()
            .map(|row| (row.get(0), Row::from_row(row, 1)))
            .collect();
        for (key, row) in &rows {
            self.remember(key.clone(), row.clone());
        }
        Ok(rows)
    }

    /// The live row at `key` as committed, read without a lock. A decided claim reads this
    /// way while it holds the head ([`Tx::hold_head`]), so no one else can change it before the
    /// transaction ends; the rows it has locked by then are its claimed rows, unchanged.
    pub(super) fn peek(&mut self, key: &str) -> Result<Option<Row>, Error> {
        let now = Some(self.now);
        let s: &mut Session = &mut self.session;
        let row = s.client.query_opt(&s.st.get, &[&key, &now])?;
        Ok(row.map(|row| Row::from_row(&row, 0)))
    }

    /// The live rows under `prefix` as committed, in key order, read without a lock, as
    /// [`Tx::peek`] reads one.
    pub(super) fn read_prefix(&mut self, prefix: &str) -> Result<Vec<(String, Row)>, Error> {
        let now = Some(self.now);
        let s: &mut Session = &mut self.session;
        let rows = s.client.query(&s.st.scan, &[&prefix, &now])?;
        Ok(rows
            .iter()
            .map(|row| (row.get(0), Row::from_row(row, 1)))
            .collect())
    }

    fn live(&self, key: &str) -> Option<&Row> {
        self.cache.get(key).and_then(Option::as_ref)
    }

    /// Applies one transaction on top of what this database transaction has done so far.
    /// Every precondition is judged before the first write, so a conflict writes nothing.
    /// The commit's revision is provisional until the tail settles it ([`Revs::settle`]).
    pub(super) fn apply(&mut self, txn: &Txn) -> Result<Result<Commit, Conflict>, Error> {
        self.load(&keys_of(std::slice::from_ref(txn)))?;
        for (index, op) in txn.ops().iter().enumerate() {
            let Some((key, cond)) = op.key_cond() else {
                continue;
            };
            let row = self.live(key);
            let met = match cond {
                Cond::Any => true,
                Cond::Absent => row.is_none(),
                Cond::Version(v) => {
                    row.is_some_and(|row| i64::try_from(v.0).is_ok_and(|v| v == row.version))
                }
            };
            if !met {
                return Ok(Err(Conflict {
                    index,
                    key: key.to_owned(),
                }));
            }
        }

        let mut counters = Vec::new();
        for op in txn.ops() {
            match op {
                Op::Put {
                    key, value, ttl, ..
                } => {
                    let expires = ttl.map(|ttl| self.now.saturating_add(micros(ttl)));
                    let at = self.record(key, KIND_PUT, Some(value.clone()));
                    self.cache.insert(
                        key.clone(),
                        Some(Row {
                            value: value.clone(),
                            version: -at,
                            expires_us: expires,
                        }),
                    );
                }
                Op::Delete { key, .. } => {
                    if self.live(key).is_some() {
                        self.record(key, KIND_DELETE, None);
                        self.cache.insert(key.clone(), None);
                    }
                }
                Op::Check { .. } => {}
                Op::Next { counter } => {
                    let s: &mut Session = &mut self.session;
                    let value: i64 = s
                        .client
                        .query_one(&s.st.counter, &[&counter.as_str()])?
                        .get(0);
                    counters.push(u64::try_from(value).unwrap_or(0));
                    self.kept = true;
                }
            }
        }
        Ok(Ok(Commit {
            revision: Revision(self.writes.len() as u64),
            counters,
        }))
    }
}

/// The keys the operations of `batch` name, in the order they name them.
pub(super) fn keys_of(batch: &[Txn]) -> Vec<&str> {
    batch
        .iter()
        .flat_map(Txn::ops)
        .filter_map(|op| op.key_cond().map(|(key, _)| key))
        .collect()
}

impl Drop for Tx<'_> {
    fn drop(&mut self) {
        if self.open && self.session.client.batch_execute("ROLLBACK").is_err() {
            self.session.broken = true;
        }
    }
}
