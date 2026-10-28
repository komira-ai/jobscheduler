//! What the core reports about itself, for the server's metrics.

use std::time::Duration;

/// Told about the core's own behaviour. Called under the core's lock, so a method neither
/// blocks nor calls back into the core: it bumps a counter or a histogram.
pub trait Observer: Send + Sync {
    /// A submit waited `waited` for the core's lock. Observed in a submit only: a lock
    /// wait of a heartbeat, a result or a tick is not reported.
    fn lock_waited(&self, waited: Duration);
}

/// Observes nothing.
impl Observer for () {
    fn lock_waited(&self, _: Duration) {}
}
