//! [`Written`]: where a job store's writes of the job tables, and the store's committed head at
//! the last commit that changed a job table, stood when it was asked ([`JobStore::written`]), so
//! a caller that read the tables can tell that they have not changed since without reading them
//! again; and [`Step`]: what one commit of the job store's own moved them by, so a caller that
//! knows what the commit wrote can bring the tables it read up to date without reading them
//! again. A host's node row (`node/<host>`) is not a job table: every heartbeat writes its
//! host's row, and those writes alone leave a reading as it was.

use std::sync::PoisonError;
use std::sync::atomic::Ordering;

use js_state::{Commit, Conflict, Op, Revision, Store, StoreError, StoreTime, Txn};

use super::JobStore;
use crate::error::JobStoreError;
use crate::ids::{IdSource, JobId};
use crate::keys;

/// How many rows of the store's feed one reading looks at past the head it last read: more
/// than that since then counts as a change of the tables.
const FEED_PAGE: usize = 512;

/// How many transactions one store batch of [`JobStore::write_each`] carries. Each is one
/// row's (with its index entries), so a batch locks about this many rows for one round trip.
pub(crate) const GROUP: usize = 100;

/// What one write of the job store answers: its commit with the [`Step`] it made, or the
/// conflict that kept it from committing.
pub(super) type Stepped = Result<(Commit, Step), Conflict>;

/// How many rows `txn` writes, each at a revision of its own, when every operation of it is a
/// put; `None` otherwise ([`Step::rows`]).
fn row_count(txn: &Txn) -> Option<u64> {
    let puts = txn.ops().iter().all(|op| matches!(op, Op::Put { .. }));
    puts.then_some(txn.change_count())
}

/// The text of the failure a stamped write answers when the store applied nothing although its
/// transaction was built, which a store that keeps [`Store::txn_stamped`]'s contract never does.
const DECLINED: &str = "the store declined a transaction that was built";

/// The store's head as the last reading read it, and the head at the last commit that changed
/// a job table as far as the readings since have told.
#[derive(Debug, Default)]
pub(super) struct Heads {
    /// The store's head at the last reading.
    seen: Option<Revision>,
    /// The head a reading gives: the store's head when the last change of a job table was
    /// seen.
    tables: Option<Revision>,
}

/// Where the writes of one job store to its job tables, and the store under it, stood when it was
/// asked ([`JobStore::written`]): two that are equal were taken from the same job store with no
/// write of its tables in between and, when the store tells its head, with no commit to a job
/// table by anyone in between (commits of node rows alone aside), so its tables held the same
/// rows at both. Taken from two job stores, they are never equal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Written {
    /// The job store's number among those this process made.
    instance: u64,
    /// How many writes of its job tables it had sent to its store.
    writes: u64,
    /// The store's committed head revision at the last commit that changed a job table, as far
    /// as the feed told, when the store tells a head ([`Store::head`]): what another server's
    /// commit to a job table moves.
    head: Option<Revision>,
}

/// One commit a job store sent ([`JobStore::insert_queued`] gives the one that recorded a job),
/// with where it left the job store's writes and the store's head.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Step {
    /// The job store's number among those this process made.
    instance: u64,
    /// How many writes it had counted before this one.
    writes: u64,
    /// The highest revision the commit wrote.
    revision: Revision,
    /// How many rows the commit wrote, each at a revision of its own; `None` when its
    /// transaction held an operation other than a put, which may take a revision and write no
    /// row, so the revisions it took are not known.
    rows: Option<u64>,
}

impl Step {
    /// What [`JobStore::written`] reads just after this step's commit, when `at` is the reading
    /// it follows: one of the same job store, with every write before this one counted and none
    /// after, and with the store's head at the revision just below the commit's first. It
    /// relies on what the store's head promises ([`Store::head`]): every revision at or below
    /// it has committed, and a commit's rows take consecutive revisions that the head passes
    /// at once when the commit lands (Postgres moves it under a lock at the commit). So no
    /// other commit can come between such a reading and this one; the tables at the answer
    /// are those at `at` with this commit's rows written, and nothing else. (The in-memory
    /// fake's `Fake::prepare`, a test's second writer, moves the head before its commit
    /// lands, and so breaks that promise.)
    ///
    /// `None` when `at` is not that reading, when the commit's transaction held an operation
    /// other than a put, and when the store does not tell its head: its count of this job
    /// store's writes alone cannot rule out a write committed between `at` and this one and
    /// counted after it.
    #[must_use]
    pub fn after(&self, at: Written) -> Option<Written> {
        let first = self.revision.0.checked_sub(self.rows?)?;
        let before = Written {
            instance: self.instance,
            writes: self.writes,
            head: Some(Revision(first)),
        };
        (at == before).then_some(Written {
            writes: self.writes.saturating_add(1),
            head: Some(self.revision),
            ..before
        })
    }
}

impl<S: Store, I: IdSource> JobStore<S, I> {
    /// Sends `txn` to the store and counts it ([`JobStore::written`]): the one way this job
    /// store writes the job tables once it is made, but for [`JobStore::place_claimed`]. A commit
    /// comes with the [`Step`] it made.
    ///
    /// A guarded view ([`JobStore::guarded`]) adds the check of its node row to `txn`, and a
    /// conflict on that row is [`JobStoreError::NodeMoved`]. A job store that is not guarded
    /// sends `txn` as it is.
    pub(super) fn write_stepped(&self, txn: Txn) -> Result<Stepped, JobStoreError> {
        let txn = self.guard_txn(txn);
        let rows = row_count(&txn);
        let answer = self.store.txn(txn);
        let writes = self.writes.fetch_add(1, Ordering::SeqCst);
        self.stepped(answer?, writes, rows)
    }

    /// [`JobStore::write_stepped`] of the transaction `build` makes from the store's clock as
    /// the transaction reads it ([`Store::txn_stamped`]), with what `build` made beside it (the
    /// record the transaction writes, stamped with that reading). When the store runs the
    /// transaction again, `build` is called again and its last answer is the one given.
    ///
    /// # Errors
    /// What `build` failed with, and nothing is written; as for [`JobStore::write_stepped`].
    pub(super) fn write_stamped<T>(
        &self,
        build: &mut dyn FnMut(StoreTime) -> Result<(Txn, T), JobStoreError>,
    ) -> Result<(T, Stepped), JobStoreError> {
        let (mut made, mut failed, mut rows) = (None, None, None);
        let answer = self.store.txn_stamped(&mut |now| {
            let (txn, value) = build(now).map_err(|error| failed = Some(error)).ok()?;
            let txn = self.guard_txn(txn);
            rows = row_count(&txn);
            made = Some(value);
            Some(txn)
        });
        let writes = self.writes.fetch_add(1, Ordering::SeqCst);
        failed.map_or(Ok(()), Err)?;
        // `build` answered, so the store applied what it built.
        let declined = JobStoreError::Store(StoreError::Failed(DECLINED.to_owned()));
        let (answer, value) = answer?.zip(made).ok_or(declined)?;
        Ok((value, self.stepped(answer, writes, rows)?))
    }

    /// The answer of a write that `writes` writes came before, of a transaction of `rows` rows.
    fn stepped(
        &self,
        answer: Result<Commit, Conflict>,
        writes: u64,
        rows: Option<u64>,
    ) -> Result<Stepped, JobStoreError> {
        match answer {
            Ok(commit) => {
                let step = Step {
                    instance: self.instance,
                    writes,
                    revision: commit.revision,
                    rows,
                };
                Ok(Ok((commit, step)))
            }
            Err(conflict) => self.refuse_moved(conflict).map(Err),
        }
    }

    /// Commits each transaction of `txns`, in order, on its own preconditions, [`GROUP`] to a
    /// store batch ([`Store::apply_batch`]: one round trip and one sync for the batch), and
    /// answers, for each in order, whether it committed. Each counts as one write
    /// ([`JobStore::written`]) and carries a guarded view's check of its node row. A lost
    /// precondition loses that transaction alone: the others of its batch commit.
    ///
    /// # Errors
    /// [`JobStoreError::NodeMoved`] on a guarded view whose node row moved;
    /// [`JobStoreError::Store`].
    pub(super) fn write_each(&self, txns: Vec<Txn>) -> Result<Vec<bool>, JobStoreError> {
        let mut held = Vec::with_capacity(txns.len());
        let mut txns = txns.into_iter().map(|txn| self.guard_txn(txn)).peekable();
        while txns.peek().is_some() {
            let batch: Vec<Txn> = txns.by_ref().take(GROUP).collect();
            let sent = u64::try_from(batch.len()).unwrap_or(u64::MAX);
            let answers = self.store.apply_batch(batch);
            self.writes.fetch_add(sent, Ordering::SeqCst);
            for answer in answers? {
                held.push(match answer {
                    Ok(_) => true,
                    Err(conflict) => self.refuse_moved(conflict).map(|_| false)?,
                });
            }
        }
        Ok(held)
    }

    /// [`JobStore::write_stepped`], without the step.
    pub(super) fn write(&self, txn: Txn) -> Result<Result<Commit, Conflict>, JobStoreError> {
        Ok(self.write_stepped(txn)?.map(|(commit, _)| commit))
    }

    /// Commits `txn`, which writes only a host's node row, without counting it as a write of
    /// the job tables ([`JobStore::written`]): every heartbeat writes its host's row, and a
    /// reader of the tables is not to read them again for that.
    pub(super) fn write_node(&self, txn: Txn) -> Result<Result<Commit, Conflict>, JobStoreError> {
        let answer = self.store.txn(self.guard_txn(txn));
        match answer? {
            Err(conflict) => self.refuse_moved(conflict).map(Err),
            committed => Ok(committed),
        }
    }

    /// Commits `txn`, whose preconditions are the versions of the job `id`'s rows the caller
    /// read: a conflict is [`JobStoreError::Stale`].
    pub(super) fn commit(&self, id: &JobId, txn: Txn) -> Result<Commit, JobStoreError> {
        self.write(txn)?
            .map_err(|_| JobStoreError::Stale { id: id.clone() })
    }

    /// Where this job store's writes of the job tables stand, and where the store's head stood at
    /// the last commit that changed a job table: each transaction it sends counts once, after
    /// the store answered it, whether it committed, conflicted or failed (a failed commit may
    /// still have landed), except a write of a host's node row alone; and the head is taken from
    /// the store's head, moved only when the feed's rows since the last reading hold a key that
    /// is not a node row ([`JobStore::tables_head`]). The tables change only through such a
    /// commit, so when two readings are equal the tables held the same rows at both, while the
    /// heartbeats' node writes leave a reading as it was. A store that does not tell its head
    /// ([`Store::head`]) is one only this process writes, and its own writes are the count. The
    /// write count is kept in memory and starts again with each job store; the head is one small
    /// read, and a page of the feed when it moved.
    ///
    /// # Errors
    /// [`JobStoreError::Store`] when the store's head cannot be read.
    pub fn written(&self) -> Result<Written, JobStoreError> {
        Ok(Written {
            instance: self.instance,
            writes: self.writes.load(Ordering::SeqCst),
            head: self.tables_head()?,
        })
    }

    /// The store's head at the last commit that changed a job table, as far as the store's feed
    /// tells, or `None` for a store that tells no head. When the head moved since the last
    /// reading, the feed's rows since are read: a page whose every row is a node row leaves the
    /// answer as it was; a row of any other key, more rows than one page, or a feed that cannot
    /// be read (trimmed past the last reading) moves it to the head now. Only ever moved too
    /// often, never too seldom: a reader then reads the tables again for nothing.
    fn tables_head(&self) -> Result<Option<Revision>, JobStoreError> {
        let Some(head) = self.store.head()? else {
            return Ok(None);
        };
        let mut heads = self.heads.lock().unwrap_or_else(PoisonError::into_inner);
        let moved = match heads.seen {
            Some(seen) if seen == head => false,
            Some(seen) => self.store.changes(seen, FEED_PAGE).map_or(true, |page| {
                let tables = page.rows.iter().any(|row| !row.key.starts_with(keys::NODE));
                tables || page.cursor < head
            }),
            None => true,
        };
        if moved {
            heads.tables = Some(head);
        }
        heads.seen = Some(head);
        Ok(heads.tables)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use js_state::fake::{Fake, Flaw};
    use js_state::{Cond, ManualClock, Revision, Store, StoreError, Txn};

    use super::{FEED_PAGE, JobStore, Step, Written};
    use crate::NewJob;
    use crate::error::JobStoreError;
    use crate::ids::{JobId, RandomIds};
    use crate::interfere::Interfere;

    /// A job store over the in-memory fake, with the fake, for a second job store over it.
    fn opened() -> (Arc<Fake>, JobStore<Fake, RandomIds>) {
        let store = Arc::new(Fake::with_flaw(
            Arc::new(ManualClock::default()),
            Flaw::None,
        ));
        let jobs = JobStore::new(Arc::clone(&store), RandomIds).expect("opens");
        (store, jobs)
    }

    /// A job to queue, with the id `id` or none (the store mints one).
    fn job(id: Option<&str>) -> NewJob {
        NewJob {
            id: id.map(|id| JobId::parse(id).expect("an id")),
            spec: crate::suite::full_spec(),
            priority: 0,
            client: None,
        }
    }

    /// The step of the commit that recorded `new` in `jobs`.
    fn recorded(jobs: &JobStore<Fake, RandomIds>, new: NewJob) -> Step {
        let inserted = jobs.insert_queued(new).expect("records");
        inserted
            .step
            .expect("a call that records the job gives its step")
    }

    fn reading(jobs: &JobStore<Fake, RandomIds>) -> Written {
        jobs.written().expect("the head reads")
    }

    /// Asserts that the step of the commit that recorded a job, with an id or a minted one,
    /// follows the reading taken just before it and answers exactly the reading taken just
    /// after it, and follows no reading taken after it. Catches a step that names the wrong
    /// head or the wrong count of writes: a placer would then add a job to tables that missed
    /// another write, or never add it.
    #[test]
    fn a_step_follows_the_reading_just_before_its_commit_and_answers_the_one_after() {
        let (_, jobs) = opened();
        for id in [Some("named"), None] {
            let before = reading(&jobs);
            let step = recorded(&jobs, job(id));
            let after = reading(&jobs);
            assert_ne!(before, after, "the commit moved the reading");
            assert_eq!(step.after(before), Some(after), "{id:?}");
            assert_eq!(step.after(after), None, "{id:?}");
        }
    }

    /// Asserts that a step follows no reading with another commit between it and the step's:
    /// one of another job store over the same store (another server), or one of the same job
    /// store (another call of this server). Catches a step judged by the write count alone,
    /// which misses another server's commit, or by the head alone, which a store that tells
    /// none cannot judge.
    #[test]
    fn a_step_follows_no_reading_with_another_commit_between() {
        let (store, jobs) = opened();
        let other = JobStore::new(Arc::clone(&store), RandomIds).expect("opens");
        let before = reading(&jobs);
        other.insert_queued(job(Some("there"))).expect("records");
        let step = recorded(&jobs, job(Some("here")));
        assert_eq!(
            step.after(before),
            None,
            "another server's commit came between"
        );
        let before = reading(&jobs);
        recorded(&jobs, job(Some("first")));
        let step = recorded(&jobs, job(Some("second")));
        assert_eq!(
            step.after(before),
            None,
            "a commit of this job store came between"
        );
    }

    /// Asserts that a call that records no job, a retry of a job already recorded, gives no
    /// step: it committed nothing that could be added to tables read before it.
    #[test]
    fn a_retry_that_records_nothing_gives_no_step() {
        let (_, jobs) = opened();
        recorded(&jobs, job(Some("once")));
        let again = jobs.insert_queued(job(Some("once"))).expect("answers");
        assert_eq!((again.created, again.step), (false, None));
    }

    /// Asserts the boundaries of a step: a commit whose transaction holds a delete follows no
    /// reading, as a delete of an absent key may take a revision and write no row, so the head
    /// before it is not known; a reading of a store that tells no head is never followed; and
    /// a step whose rows outnumber its revision, which no store gives, follows nothing. The
    /// same commit of puts alone, from the same reading, is followed.
    #[test]
    fn a_step_of_a_delete_or_from_a_reading_with_no_head_follows_nothing() {
        let (_, jobs) = opened();
        let before = reading(&jobs);
        let txn = Txn::new()
            .put("test/a", b"1".to_vec(), Cond::Any)
            .delete("test/b", Cond::Any);
        let (_, step) = jobs.write_stepped(txn).expect("reaches").expect("commits");
        assert_eq!(step.after(before), None, "a delete came with the puts");
        let before = reading(&jobs);
        let txn = Txn::new().put("test/c", b"1".to_vec(), Cond::Any);
        let (_, step) = jobs.write_stepped(txn).expect("reaches").expect("commits");
        let headless = Written {
            head: None,
            ..before
        };
        assert_eq!(step.after(headless), None, "a reading with no head");
        assert_eq!(step.after(before), Some(reading(&jobs)), "puts alone");
        let short = Step {
            revision: Revision(1),
            rows: Some(2),
            ..step
        };
        assert_eq!(short.after(before), None, "more rows than revisions");
    }

    // Catches a reading that answers a mark without the store's head when the head cannot be
    // read, or answers some error other than the store's.
    #[test]
    fn a_head_that_cannot_be_read_fails_the_reading_with_the_stores_error() {
        let store = Arc::new(Interfere::new(Fake::with_flaw(
            Arc::new(ManualClock::default()),
            Flaw::None,
        )));
        let jobs = JobStore::new(Arc::clone(&store), RandomIds).expect("opens");
        let before = jobs.written().expect("the head reads");
        store.fail_with(StoreError::Disconnected("the head read failed".to_owned()));
        store.fail_next_head();
        assert_eq!(
            jobs.written(),
            Err(JobStoreError::Store(StoreError::Disconnected(
                "the head read failed".to_owned()
            )))
        );
        assert_eq!(
            jobs.written(),
            Ok(before),
            "only the one head read fails, and nothing was written in between"
        );
    }

    /// A job store over a fake store that tells its head, and the store.
    fn tables() -> (Arc<Fake>, JobStore<Fake, RandomIds>) {
        let store = Arc::new(Fake::with_flaw(
            Arc::new(ManualClock::default()),
            Flaw::None,
        ));
        let jobs = JobStore::new(Arc::clone(&store), RandomIds).expect("opens");
        (store, jobs)
    }

    /// Commits a put of each of `keys`, in one transaction.
    fn put(store: &Fake, keys: &[String]) {
        let txn = keys
            .iter()
            .fold(Txn::new(), |txn, key| txn.put(key.as_str(), "v", Cond::Any));
        store.txn(txn).expect("the store").expect("no conflict");
    }

    // Catches a reading that moves with every heartbeat (each writes its host's node row), so a
    // pass after a quiet beat reads the tables again and runs every round, and one that does not
    // move when a job table changes: node rows written directly or through the job store leave
    // the reading as it was, and a job row moves it.
    #[test]
    fn node_rows_leave_the_reading_and_a_job_table_moves_it() {
        let (store, jobs) = tables();
        let before = jobs.written().expect("reads");
        assert_eq!(jobs.written(), Ok(before), "nothing was written");
        put(&store, &["node/host-a".to_owned()]);
        assert_eq!(jobs.written(), Ok(before), "a node row moved the reading");
        let host = js_types::HostId::new("host-b").expect("an id");
        let hello = crate::NodeHello {
            session: None,
            pool: "pool".to_owned(),
            claim: None,
            shape: None,
            labels: std::collections::BTreeMap::new(),
            phase: js_agent_wire::Phase::Serving,
            agent_process: None,
            agent_version: None,
        };
        jobs.hello_node(&host, &hello).expect("a hello");
        assert_eq!(
            jobs.written(),
            Ok(before),
            "the job store's node write moved it"
        );
        put(&store, &["node/host-c".to_owned(), "job/x".to_owned()]);
        let after = jobs.written().expect("reads");
        assert_ne!(after, before, "a job row did not move the reading");
        assert_eq!(jobs.written(), Ok(after));
    }

    // Catches a reading that judges only the first page of the feed: more node rows since the
    // last reading than one page holds moves it, since what lies past the page is not known.
    #[test]
    fn more_rows_than_a_page_move_the_reading() {
        let (store, jobs) = tables();
        let before = jobs.written().expect("reads");
        let keys: Vec<String> = (0..=FEED_PAGE).map(|n| format!("node/host-{n}")).collect();
        put(&store, &keys);
        assert_ne!(jobs.written(), Ok(before));
    }

    // Catches a reading that answers "unchanged" when the feed since the last reading cannot be
    // read (it was trimmed): a job table may have changed in the rows it lost.
    #[test]
    fn a_feed_trimmed_past_the_last_reading_moves_it() {
        let (store, jobs) = tables();
        let before = jobs.written().expect("reads");
        put(&store, &["node/host-a".to_owned()]);
        let head = store.head().expect("reads").expect("a head");
        store.trim_feed(head).expect("trims");
        assert_ne!(jobs.written(), Ok(before));
    }
}
