//! [`Wake`]: what the work the lifecycle hands off its worker (an image's pull, a lookup or
//! an assembly of a job's file system, a provider's call) rings when it is done, so that
//! the worker takes the start that waits on it one step on at once instead of at its next
//! step interval.

use std::fmt;
use std::sync::Arc;

/// Asks the lifecycle's worker for a step, without waiting. A clone rings the same worker.
/// The default rings nobody: a lifecycle with no worker behind it (a test that steps it by
/// hand) is stepped by its caller.
#[derive(Clone, Default)]
pub struct Wake(Option<Arc<dyn Fn() + Send + Sync>>);

impl Wake {
    /// A wake that calls `ring`, which must not wait (it is called from the threads of the
    /// work handed off, some of which hold a lock of their own).
    pub fn new(ring: impl Fn() + Send + Sync + 'static) -> Self {
        Self(Some(Arc::new(ring)))
    }

    /// Asks for a step.
    pub fn wake(&self) {
        if let Some(ring) = &self.0 {
            ring();
        }
    }
}

impl fmt::Debug for Wake {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rings = match self.0 {
            Some(_) => "a worker",
            None => "nobody",
        };
        f.debug_tuple("Wake").field(&rings).finish()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    #[test]
    fn a_wake_rings_its_worker_once_for_each_wake_and_a_clone_rings_the_same() {
        let rings = Arc::new(AtomicU32::new(0));
        let counted = Arc::clone(&rings);
        let wake = Wake::new(move || {
            counted.fetch_add(1, Ordering::SeqCst);
        });
        wake.wake();
        wake.clone().wake();
        assert_eq!(rings.load(Ordering::SeqCst), 2);
        assert_eq!(format!("{wake:?}"), "Wake(\"a worker\")");
    }

    #[test]
    fn the_default_wake_rings_nobody() {
        let wake = Wake::default();
        wake.wake();
        assert_eq!(format!("{wake:?}"), "Wake(\"nobody\")");
    }
}
