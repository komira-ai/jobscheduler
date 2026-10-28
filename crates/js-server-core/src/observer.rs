//! What the core reports about itself, for the server's metrics.

use std::time::Duration;

/// Told about the core's own behaviour. Called outside the core's lock.
pub trait Observer: Send + Sync {
    /// A submit waited `waited` for the core's lock. Observed in a submit only: a lock
    /// wait of a heartbeat, a result or a tick is not reported.
    fn lock_waited(&self, waited: Duration);
}

/// Observes nothing.
impl Observer for () {
    fn lock_waited(&self, _: Duration) {}
}
