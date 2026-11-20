//! The removal of a finished run's directories (its scratch directory, the runtime's lease
//! and temporary directories beside it, its files directory), off the worker when the
//! lifecycle has threads for it ([`Lifecycle::with_cleanup_threads`], `js-scratch`).
//!
//! A run's scratch directory holds what its job wrote, and its removal is one system call
//! for each entry: a large tree takes seconds. On the worker it held every other job of the
//! host for that long, a start's admission among them. With threads the removal is asked of
//! the pool, the job waits in its stage (`Scratch`) with its slot booked, the pool rings the
//! worker's wake when it is done, and the step after takes the answer. Without threads (a
//! lifecycle a test steps by hand) the removal runs on the caller's thread, as before.
//!
//! A removal has no deadline: one that hangs (a disk that never answers) holds its thread
//! and its job. One whose thread ends without an answer (it panicked) is a failure of the
//! removal, and the next step removes the directories again.
//!
//! [`Lifecycle::with_cleanup_threads`]: super::Lifecycle::with_cleanup_threads

use std::collections::BTreeMap;
use std::io;

use crate::lifecycle::Wake;
use crate::pool::{Pending, Poll, Pool};

/// What a thread of the pool runs: the removal of one run's directories.
pub(super) type Task = Box<dyn FnOnce() -> io::Result<()> + Send>;

/// How the removal of a run's directories stands.
#[derive(Debug)]
pub(super) enum DirsRemoval {
    /// Under way on the pool: ask again at the next step.
    Pending,
    /// Done: the directories are gone, or the first failure.
    Done(io::Result<()>),
}

/// The pool, if the lifecycle has one, and the removals under way on it, by run.
#[derive(Default)]
pub(super) struct RunDirs {
    pool: Option<Pool>,
    under_way: BTreeMap<String, Pending<io::Result<()>>>,
}

impl RunDirs {
    /// Removals on a pool of `threads` threads named `js-scratch`, which rings `wake` when
    /// one has answered.
    ///
    /// # Errors
    /// A thread could not be made.
    pub(super) fn pooled(threads: usize, wake: Wake) -> io::Result<Self> {
        let mut pool = Pool::named(threads, "js-scratch")?;
        pool.set_wake(wake);
        Ok(Self {
            pool: Some(pool),
            under_way: BTreeMap::new(),
        })
    }

    /// Rings `wake` whenever a removal begun from now on has answered.
    pub(super) fn set_wake(&mut self, wake: &Wake) {
        self.pool
            .iter_mut()
            .for_each(|pool| pool.set_wake(wake.clone()));
    }

    /// How the removal of the directories of `run_id` under way stands; `None` when none is.
    /// Once it answered it is forgotten.
    pub(super) fn follow(&mut self, run_id: &str) -> Option<DirsRemoval> {
        let pending = self.under_way.remove(run_id)?;
        Some(match pending.poll(0) {
            Poll::Waiting => {
                self.under_way.insert(run_id.to_owned(), pending);
                DirsRemoval::Pending
            }
            Poll::Answered(removed) => DirsRemoval::Done(removed),
            Poll::Unanswered(why) => DirsRemoval::Done(Err(io::Error::other(format!(
                "the removal of the run's directories {why}"
            )))),
        })
    }

    /// Begins `task`, the removal of the directories of `run_id`: on the pool, pending, or on
    /// the caller's thread, done, for a lifecycle without one.
    pub(super) fn begin(&mut self, run_id: &str, task: Task) -> DirsRemoval {
        let Some(pool) = &self.pool else {
            return DirsRemoval::Done(task());
        };
        let pending = pool.run(u64::MAX, task);
        self.under_way.insert(run_id.to_owned(), pending);
        DirsRemoval::Pending
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::{Receiver, channel};

    use super::*;

    /// Removals on one thread, and what rings when one has answered.
    fn one_thread() -> (RunDirs, Receiver<()>) {
        let (ring, rung) = channel();
        let wake = Wake::new(move || {
            let _ = ring.send(());
        });
        (RunDirs::pooled(1, wake).expect("a pool"), rung)
    }

    /// What `answer` says, as text, for an assertion.
    fn said(answer: Option<DirsRemoval>) -> String {
        format!("{answer:?}")
    }

    /// Asserts that a removal begun on the pool is pending until its task returns, is asked
    /// again for as long as it waits, and once its thread rang answers its task's answer once:
    /// a second ask finds none under way. Catches a removal answered before it ran, one lost
    /// while it waits, and one answered twice.
    #[test]
    fn a_removal_on_the_pool_is_pending_until_it_answers_once() {
        let (mut dirs, rung) = one_thread();
        let (release, held) = channel::<()>();
        let task: Task = Box::new(move || {
            let _ = held.recv();
            Err(io::Error::other("busy"))
        });
        assert!(matches!(dirs.begin("r1", task), DirsRemoval::Pending));
        assert!(matches!(dirs.follow("r1"), Some(DirsRemoval::Pending)));
        release.send(()).expect("released");
        rung.recv().expect("rung");
        assert_eq!(
            said(dirs.follow("r1")),
            r#"Some(Done(Err(Custom { kind: Other, error: "busy" })))"#
        );
        assert!(dirs.follow("r1").is_none());
    }

    /// Asserts that a removal whose thread panicked is answered as a failure that says so, and
    /// that the pool goes on: the removal after it on the same thread answers. Catches a
    /// removal lost to a panic that stays pending for good, which would hold the run's slot.
    #[test]
    fn a_removal_whose_thread_panicked_is_a_failure() {
        let (mut dirs, rung) = one_thread();
        let lost: Task = Box::new(|| panic!("the removal panicked"));
        assert!(matches!(dirs.begin("r1", lost), DirsRemoval::Pending));
        let after: Task = Box::new(|| Ok(()));
        assert!(matches!(dirs.begin("r2", after), DirsRemoval::Pending));
        // One thread: the second removal ran after the first panicked, and only it rang.
        rung.recv().expect("rung");
        assert_eq!(
            said(dirs.follow("r1")),
            r#"Some(Done(Err(Custom { kind: Other, error: "the removal of the run's directories ended without an answer" })))"#
        );
        assert!(matches!(dirs.follow("r2"), Some(DirsRemoval::Done(Ok(())))));
    }

    /// Asserts that without a pool a removal runs on the caller's thread and is answered at
    /// once, and that nothing is left under way. Catches a lifecycle without threads whose
    /// removal is pending for good.
    #[test]
    fn without_a_pool_a_removal_runs_at_once() {
        let mut dirs = RunDirs::default();
        dirs.set_wake(&Wake::default());
        let task: Task = Box::new(|| Ok(()));
        assert!(matches!(dirs.begin("r1", task), DirsRemoval::Done(Ok(()))));
        assert!(dirs.follow("r1").is_none());
    }
}
