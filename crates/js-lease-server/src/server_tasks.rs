//! The tasks a running server ([`super::Running`]) holds, each ended when it is dropped.

use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// The tick task or the placement poller, ended when it is dropped: a server that is dropped
/// must not go on ticking or placing.
pub(super) struct Ticking(pub(super) JoinHandle<()>);

/// The placement poller's task, with the signal that stops it between two passes: an orderly
/// stop ([`super::Running::shutdown`]) waits for a pass under way to end, so nothing the pass
/// holds outlives the stop. Dropped, it is aborted.
pub(super) struct Polling {
    /// Sent to stop the poller once its pass under way, if any, has ended.
    pub(super) stop: oneshot::Sender<()>,
    pub(super) task: Ticking,
}

impl Drop for Ticking {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The check loop of the elastic pool, ended when it is dropped.
pub(super) struct Checking(pub(super) Option<JoinHandle<()>>);

impl Drop for Checking {
    fn drop(&mut self) {
        self.0.iter().for_each(JoinHandle::abort);
    }
}
