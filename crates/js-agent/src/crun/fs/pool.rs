//! The `js-paths` pool: the threads that make the calls of a job's file system that may
//! wait on a provider's file system (a lookup of a host path, the mounts of an assembly, the
//! check of a job's sources after it ended), off the lifecycle's worker.
//!
//! A call on a FUSE file system blocks in the kernel until the file system answers, and one
//! that has hung never returns. The pool has a fixed number of threads and makes none on
//! demand, so a hung provider holds at most those threads; every call waits for a thread
//! in the order it was asked, and the caller gives up on its answer at a deadline of its
//! own clock ([`Pending::poll`]) whether or not a thread ever took it. A thread stuck in the
//! kernel stays stuck until the file system answers or is unmounted.

use std::io;
use std::panic::AssertUnwindSafe;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex, PoisonError};

/// The threads of the pool of a node.
pub(crate) const THREADS: usize = 4;

/// What a thread of the pool runs.
type Task = Box<dyn FnOnce() + Send>;

/// The pool: a queue of calls and the threads that take them.
#[derive(Debug)]
pub(crate) struct Pool {
    work: Sender<Task>,
}

impl Pool {
    /// A pool of `threads` threads named `js-paths`.
    ///
    /// # Errors
    /// A thread could not be made.
    pub(crate) fn new(threads: usize) -> io::Result<Self> {
        let (work, queue) = mpsc::channel::<Task>();
        let queue = Arc::new(Mutex::new(queue));
        let spawned = (0..threads).try_for_each(|_| {
            let queue = Arc::clone(&queue);
            let thread = std::thread::Builder::new().name("js-paths".to_owned());
            thread.spawn(move || serve(&queue)).map(drop)
        });
        spawned.map(|()| Self { work })
    }

    /// Asks a thread of the pool to run `task`, whose answer the caller waits for until
    /// `deadline_ms` of its own clock.
    pub(crate) fn run<T: Send + 'static>(
        &self,
        deadline_ms: u64,
        task: impl FnOnce() -> T + Send + 'static,
    ) -> Pending<T> {
        let (answer, answered) = mpsc::channel();
        // A queue no thread takes from any more drops the task, and its answer with it:
        // the caller hears that it is lost.
        let _ = self.work.send(Box::new(move || {
            let _ = answer.send(task());
        }));
        Pending {
            answered,
            deadline_ms,
        }
    }
}

/// What a thread of the pool does: takes the next task and runs it, until the pool is
/// gone. A task that panics loses its answer and not the thread.
fn serve(queue: &Mutex<Receiver<Task>>) {
    while let Ok(task) = next(queue) {
        let _ = std::panic::catch_unwind(AssertUnwindSafe(task));
    }
}

/// The next task of the queue, waiting for one; an error once the pool is gone.
fn next(queue: &Mutex<Receiver<Task>>) -> Result<Task, mpsc::RecvError> {
    queue.lock().unwrap_or_else(PoisonError::into_inner).recv()
}

/// The answer of a task of the pool, to come.
#[derive(Debug)]
pub(crate) struct Pending<T> {
    answered: Receiver<T>,
    deadline_ms: u64,
}

/// How a [`Pending`] answer stands.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Poll<T> {
    /// The task answered. A later poll of the same answer finds it lost.
    Answered(T),
    /// No answer yet, and the deadline has not passed.
    Waiting,
    /// No answer is to be waited for: the deadline passed, or the task ended without one.
    Unanswered(&'static str),
}

impl<T> Pending<T> {
    /// The answer, if it has come, as of `now_ms` on the clock the deadline was set on.
    pub(crate) fn poll(&self, now_ms: u64) -> Poll<T> {
        match self.answered.try_recv() {
            Ok(answer) => Poll::Answered(answer),
            Err(missing) => match unanswered(&missing, now_ms, self.deadline_ms) {
                None => Poll::Waiting,
                Some(why) => Poll::Unanswered(why),
            },
        }
    }
}

/// Why an answer that is `missing` at `now_ms` is not to be waited for any more, against
/// `deadline_ms`; `None` while it is.
fn unanswered(missing: &TryRecvError, now_ms: u64, deadline_ms: u64) -> Option<&'static str> {
    match missing {
        TryRecvError::Empty if now_ms < deadline_ms => None,
        TryRecvError::Empty => Some("did not answer in time"),
        TryRecvError::Disconnected => Some("ended without an answer"),
    }
}
