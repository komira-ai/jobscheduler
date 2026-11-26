//! The one way a unit test of this crate captures what the code logs: [`capture`].
//!
//! A test that installs a subscriber for its own thread (`set_default`) and rebuilds the
//! callsites' interest updates state that every thread shares: the cached interest of each
//! callsite and the global maximum level. Two such tests running at once can each rebuild
//! from a list of subscribers that the other is changing, and the last to store its result
//! can leave the maximum level below the other test's, whose events are then filtered out
//! before any subscriber sees them. So every capture holds one lock for as long as its
//! subscriber is installed, and captures run one at a time.

use std::io::Write;
use std::sync::{Mutex, MutexGuard, PoisonError};

use tracing::Level;
use tracing::subscriber::DefaultGuard;

/// Held by every [`capture`] while its subscriber is installed.
static SERIAL: Mutex<()> = Mutex::new(());

/// Waits until no [`capture`] is installed and keeps others out while held: for a test that
/// installs a subscriber of its own (`crate::warnings`'s), which races the captures as two
/// captures would.
pub(crate) fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A subscriber installed for this thread; dropping it removes the subscriber, then
/// releases the lock that keeps other captures out.
pub(crate) struct Capture {
    // Dropped first: the subscriber is gone before another capture may install its own.
    _subscriber: DefaultGuard,
    _serial: MutexGuard<'static, ()>,
}

/// Installs, for this thread, a subscriber that writes every event at `level` or above to
/// `writer` in plain text, and rebuilds the callsites' interest so that a callsite another
/// test cached earlier sees it. Waits until no other capture is installed. A capture whose
/// test panicked still releases the lock, so one failing test does not fail the others.
pub(crate) fn capture<W>(writer: W, level: Level) -> Capture
where
    W: Write + Clone + Send + Sync + 'static,
{
    let serial = serial();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .with_max_level(level)
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);
    tracing::callsite::rebuild_interest_cache();
    Capture {
        _subscriber: guard,
        _serial: serial,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, TryLockError};

    use super::*;

    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl Write for Sink {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Sink {
        fn text(&self) -> String {
            let bytes = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            String::from_utf8_lossy(&bytes).into_owned()
        }
    }

    /// A warning from a callsite of its own, reached only through this function.
    fn warn_from_one_callsite(what: &str) {
        tracing::warn!(what, "from the shared callsite");
    }

    /// Proves a capture sees an event at a callsite that another thread, with no subscriber
    /// of its own, reached first while the capture was installed: once, from this thread.
    /// Catches a capture that is the only live dispatcher, with which `tracing-core` caches
    /// the callsite as never wanted from the other thread's lack of a subscriber.
    #[test]
    fn a_callsite_another_thread_reaches_first_is_still_captured() {
        let sink = Sink::default();
        let held = capture(sink.clone(), Level::WARN);
        std::thread::spawn(|| warn_from_one_callsite("elsewhere"))
            .join()
            .expect("the other thread ends");
        warn_from_one_callsite("here");
        drop(held);
        let text = sink.text();
        assert_eq!(text.matches("from the shared callsite").count(), 1, "{text}");
        assert!(text.contains("what=\"here\""), "{text}");
    }

    /// A warning from a callsite of its own, reached only through this function.
    fn warn_around_a_rebuild(when: &str) {
        tracing::warn!(when, "around the other thread's rebuild");
    }

    /// Proves a capture still sees the events of a callsite it already reached after another
    /// thread, with no subscriber of its own, rebuilt the shared interest. Catches a capture
    /// that is the only live dispatcher, with which the rebuild caches every callsite reached
    /// so far as never wanted, from the other thread's lack of a subscriber.
    #[test]
    fn a_rebuild_on_another_thread_keeps_what_the_capture_sees() {
        let sink = Sink::default();
        let held = capture(sink.clone(), Level::WARN);
        warn_around_a_rebuild("before");
        std::thread::spawn(tracing::callsite::rebuild_interest_cache)
            .join()
            .expect("the other thread ends");
        warn_around_a_rebuild("after");
        drop(held);
        let text = sink.text();
        assert!(text.contains("when=\"before\""), "{text}");
        assert!(text.contains("when=\"after\""), "{text}");
    }

    /// Proves a capture holds the lock every capture takes for as long as it is installed,
    /// and releases it when dropped, and that it sees an event at its level and not one
    /// below it. Catches a capture that installs its subscriber without the lock, which lets
    /// two captures rebuild the shared interest at once and lose one's events.
    #[test]
    fn a_capture_is_serialised_and_sees_events_at_its_level() {
        let sink = Sink::default();
        let held = capture(sink.clone(), Level::WARN);
        assert!(matches!(SERIAL.try_lock(), Err(TryLockError::WouldBlock)));
        tracing::warn!("seen by the capture");
        tracing::info!("below its level");
        drop(held);
        assert!(SERIAL.try_lock().is_ok());
        assert!(sink.clone().flush().is_ok());
        let text = String::from_utf8_lossy(&sink.0.lock().unwrap_or_else(PoisonError::into_inner))
            .into_owned();
        assert!(text.contains("seen by the capture"), "{text}");
        assert!(!text.contains("below its level"), "{text}");
    }
}
