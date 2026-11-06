//! The mark that the state store has a newer writer.
//!
//! A store that answers [`StoreError::Fenced`] says this process is no longer its writer:
//! nothing was written, and no retry on the handle can succeed. That is not a failure the next
//! tick can mend, as a store that could not commit is, so the server does not log it and go
//! on: the site that sees it trips the [`Fence`], and everything that acts on the store reads
//! the mark. The tick loop ends, the agent routes refuse heartbeats, the final tick of a
//! shutdown writes nothing, and the process exits with an error so its supervisor starts it
//! afresh, which opens a new handle.

use std::sync::Arc;

use js_jobs::JobStoreError;
use js_state::StoreError;
use tokio::sync::watch;

/// The message of the answer the server gives once the state store has a newer writer: the
/// 503 code of a heartbeat and the `UNAVAILABLE` message of the job service.
pub(crate) const STATE_FENCED: &str = "state-fenced";

/// Whether `error` says this process is no longer the store's writer.
pub(crate) fn is_fenced(error: &JobStoreError) -> bool {
    matches!(error, JobStoreError::Store(StoreError::Fenced))
}

/// Whether the state store has told the server it is no longer the writer. Once tripped it
/// stays tripped: nothing resets it. A clone is the same fence.
#[derive(Clone)]
pub(crate) struct Fence {
    tripped: Arc<watch::Sender<bool>>,
}

impl Fence {
    /// A fence that has not tripped.
    pub(crate) fn new() -> Self {
        Self {
            tripped: Arc::new(watch::Sender::new(false)),
        }
    }

    /// Trips the fence when `error` is [`StoreError::Fenced`]; whether it was.
    pub(crate) fn note(&self, error: &JobStoreError) -> bool {
        let fenced = is_fenced(error);
        if fenced {
            self.tripped.send_replace(true);
        }
        fenced
    }

    /// Whether the fence has tripped.
    pub(crate) fn is_tripped(&self) -> bool {
        *self.tripped.borrow()
    }

    /// Completes once the fence has tripped (at once when it already has).
    pub(crate) async fn tripped(&self) {
        let mut seen = self.tripped.subscribe();
        // The sender is held by `self`, so the wait ends only by the fence tripping.
        let _ = seen.wait_for(|tripped| *tripped).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts that only the fenced error trips the fence, that it stays tripped, and that a
    /// wait on it completes once it has. It catches a fence tripped by any store failure (a
    /// retryable commit failure must not end the server) and one that resets.
    #[tokio::test]
    async fn only_a_fenced_store_trips_the_fence_and_it_stays_tripped() {
        let fence = Fence::new();
        let failed = JobStoreError::Store(StoreError::Failed("disk".to_owned()));
        assert!(!fence.note(&failed));
        assert!(!fence.is_tripped());
        assert!(fence.note(&JobStoreError::Store(StoreError::Fenced)));
        assert!(fence.is_tripped());
        assert!(!fence.note(&failed));
        assert!(fence.is_tripped());
        fence.tripped().await;
    }
}
