//! The clock the server reads time through.

use std::sync::Arc;

use js_types::{Clock, Timestamp};

/// A clock the server and its caller can both hold: clones read the same source.
///
/// The server reads time through this and nowhere else, so a test gives it a
/// [`js_types::ManualClock`] and the server's notion of time moves only when the test
/// says so.
#[derive(Clone)]
pub struct SharedClock(Arc<dyn Clock + Send + Sync>);

impl SharedClock {
    /// Shares `clock`.
    pub fn new(clock: impl Clock + Send + Sync + 'static) -> Self {
        Self(Arc::new(clock))
    }
}

impl Clock for SharedClock {
    fn now(&self) -> Timestamp {
        self.0.now()
    }
}

#[cfg(test)]
mod tests {
    use js_types::ManualClock;

    use super::*;

    /// Asserts that a shared clock reads the clock it wraps: a change of the wrapped clock
    /// after the share is seen by the share and by its clones.
    #[test]
    fn a_shared_clock_reads_the_clock_it_wraps() {
        let manual = ManualClock::new(Timestamp::from_millis(5));
        let shared = SharedClock::new(manual.clone());
        manual.set(Timestamp::from_millis(9));
        assert_eq!(shared.now(), Timestamp::from_millis(9));
        assert_eq!(shared.clone().now(), Timestamp::from_millis(9));
    }
}
