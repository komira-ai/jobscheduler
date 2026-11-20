//! The drain of a stopping server's agent side ([`crate::server::Running::hold`]): from the
//! termination signal on, the server places no job, holds no heartbeat's reply, holds no
//! dispatch poll and claims no dispatch message, so the agents it serves leave for another
//! server over the same store as soon as their next heartbeat or poll needs a connection, and
//! no job is placed on a host by a server that will not hear it again. A dispatch poll that
//! has claimed when the drain begins is answered with what it claimed, whose claim is
//! committed; a held one is answered empty at once; the acknowledgements of a poll are
//! applied as ever ([`crate::agent_api`]).
//!
//! What the server already committed stays: a job it placed before the drain is in the job
//! tables, and whichever server the host says hello to next sends its start. The job pass
//! (expiries, retention, loss) keeps running in the tick until the process exits, under its
//! claim as before.

use std::sync::Arc;

use tokio::sync::watch;

/// Whether the drain has begun, shared by the routes, the pass and the stop of one server.
#[derive(Clone)]
pub(crate) struct Drain(Arc<watch::Sender<bool>>);

impl Default for Drain {
    fn default() -> Self {
        Self(Arc::new(watch::Sender::new(false)))
    }
}

impl Drain {
    /// Begins the drain; a second call changes nothing.
    pub(crate) fn begin(&self) {
        self.0.send_replace(true);
    }

    /// Whether the drain has begun.
    pub(crate) fn begun(&self) -> bool {
        *self.0.borrow()
    }

    /// Waits until the drain has begun: at once when it has.
    pub(crate) async fn begins(&self) {
        let mut begun = self.0.subscribe();
        // The sender lives as long as `self`, so the wait ends only with the drain.
        let _ = begun.wait_for(|begun| *begun).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts that a drain is not begun until `begin`, that a wait started before it ends
    /// with it, that a wait started after it ends at once, and that a second `begin` keeps it
    /// begun. It catches a drain that a clone does not share and a wait that misses a
    /// `begin` made before it subscribed.
    #[tokio::test]
    async fn a_drain_is_seen_by_every_clone_and_every_wait() {
        let drain = Drain::default();
        let other = drain.clone();
        assert!(!other.begun(), "a new drain has not begun");
        let waiting = tokio::spawn({
            let other = other.clone();
            async move { other.begins().await }
        });
        drain.begin();
        waiting.await.unwrap();
        assert!(other.begun(), "the clone sees the drain");
        other.begins().await;
        drain.begin();
        assert!(drain.begun(), "a second begin keeps it begun");
    }
}
