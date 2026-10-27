//! Small helpers shared by the modules.

use std::future::pending;

use tokio::task::spawn_blocking;

/// `value` when there is one; otherwise a future that never completes. A channel whose
/// sender is gone has no more to say, and the caller waits on its other events (a
/// shutdown) instead of ending the server or spinning.
pub(crate) async fn or_pending<T>(value: Option<T>) -> T {
    match value {
        Some(value) => value,
        None => pending().await,
    }
}

/// Runs `work` on the runtime's pool for blocking calls and waits for it without holding
/// up a worker: for file writes and syncs, which an async handler must not do itself. A
/// panic in `work` is raised again in the caller.
pub(crate) async fn blocking<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    match spawn_blocking(work).await {
        Ok(done) => done,
        Err(failed) => std::panic::resume_unwind(failed.into_panic()),
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::sync::Mutex;
    use std::task::{Context, Poll, Waker};
    use std::thread::{self, ThreadId};

    use super::*;

    /// Asserts that the work runs on a thread of its own, not on the thread of the task
    /// that awaits it (the single thread of this test's runtime), and that a panic in
    /// the work reaches the caller as a panic. It catches a helper that calls the work
    /// inline, which would block the runtime on a file sync, and one that swallows a
    /// panic.
    #[tokio::test]
    async fn work_runs_off_the_task_and_its_panic_is_raised() {
        // Both jobs are plain functions, so one instantiation of the helper runs both.
        static RAN_ON: Mutex<Option<ThreadId>> = Mutex::new(None);
        fn record() {
            *RAN_ON.lock().unwrap() = Some(thread::current().id());
        }
        fn fail() {
            panic!("the work failed");
        }
        let jobs: [fn(); 2] = [record, fail];
        blocking(jobs[0]).await;
        let ran_on = *RAN_ON.lock().unwrap();
        assert!(ran_on.is_some());
        assert_ne!(ran_on, Some(thread::current().id()));
        let panicked = tokio::spawn(blocking(jobs[1])).await;
        assert!(panicked.unwrap_err().is_panic());
    }

    /// Asserts that a value is passed through and that no value never completes.
    #[test]
    fn passes_a_value_and_waits_for_none() {
        let mut cx = Context::from_waker(Waker::noop());
        let mut ready = std::pin::pin!(or_pending(Some(3u8)));
        assert_eq!(ready.as_mut().poll(&mut cx), Poll::Ready(3u8));
        let mut waiting = std::pin::pin!(or_pending(None::<u8>));
        assert_eq!(waiting.as_mut().poll(&mut cx), Poll::Pending);
    }
}
