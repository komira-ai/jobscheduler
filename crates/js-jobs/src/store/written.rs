//! [`Written`]: where a job store's writes, and the store's committed head, stood when it was
//! asked ([`JobStore::written`]), so a caller that read the tables can tell that they have not
//! changed since without reading them again; and [`Step`]: what one commit of the job store's
//! own moved them by, so a caller that knows what the commit wrote can bring the tables it read
//! up to date without reading them again.

use std::sync::atomic::Ordering;

use js_state::{Commit, Conflict, Op, Revision, Store, StoreError, Txn};

use super::JobStore;
use crate::error::JobStoreError;
use crate::ids::{IdSource, JobId};

/// Where the writes of one job store, and the store under it, stood when it was asked
/// ([`JobStore::written`]): two that are equal were taken from the same job store with no write
/// of it in between and, when the store tells its head, with no commit to the store by anyone
/// in between, so its tables held the same rows at both. Taken from two job stores, they are
/// never equal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Written {
    /// The job store's number among those this process made.
    instance: u64,
    /// How many writes it had sent to its store.
    writes: u64,
    /// The store's committed head revision, when it tells one ([`Store::head`]): what another
    /// server's commit moves.
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
    /// store writes once it is made. A commit comes with the [`Step`] it made.
    pub(super) fn write_stepped(
        &self,
        txn: Txn,
    ) -> Result<Result<(Commit, Step), Conflict>, StoreError> {
        let puts = txn.ops().iter().all(|op| matches!(op, Op::Put { .. }));
        let rows = puts.then_some(txn.change_count());
        let answer = self.store.txn(txn);
        let writes = self.writes.fetch_add(1, Ordering::SeqCst);
        let step = |commit: Commit| {
            let revision = commit.revision;
            let step = Step {
                instance: self.instance,
                writes,
                revision,
                rows,
            };
            (commit, step)
        };
        Ok(answer?.map(step))
    }

    /// [`JobStore::write_stepped`], without the step.
    pub(super) fn write(&self, txn: Txn) -> Result<Result<Commit, Conflict>, StoreError> {
        Ok(self.write_stepped(txn)?.map(|(commit, _)| commit))
    }

    /// Commits `txn`, whose preconditions are the versions of the job `id`'s rows the caller
    /// read: a conflict is [`JobStoreError::Stale`].
    pub(super) fn commit(&self, id: &JobId, txn: Txn) -> Result<Commit, JobStoreError> {
        self.write(txn)?
            .map_err(|_| JobStoreError::Stale { id: id.clone() })
    }

    /// Where this job store's writes stand, and where the store's head stands: each transaction
    /// it sends counts once, after the store answered it, whether it committed, conflicted or
    /// failed (a failed commit may still have landed), and the store's committed head moves with
    /// every commit, this job store's or another server's. The tables change only through such
    /// a commit, so when two readings are equal the tables held the same rows at both. A store
    /// that does not tell its head ([`Store::head`]) is one only this process writes, and its
    /// own writes are the count. The write count is kept in memory and starts again with each job
    /// store; the head is the store's, one small read.
    ///
    /// # Errors
    /// [`JobStoreError::Store`] when the store's head cannot be read.
    pub fn written(&self) -> Result<Written, JobStoreError> {
        Ok(Written {
            instance: self.instance,
            writes: self.writes.load(Ordering::SeqCst),
            head: self.store.head()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use js_state::fake::{Fake, Flaw};
    use js_state::{Cond, ManualClock, Revision, StoreError, Txn};

    use super::{JobStore, Step, Written};
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
}
