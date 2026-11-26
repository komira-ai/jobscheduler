//! The one way a unit test of this crate captures what the code logs: [`capture`].
//!
//! A capture installs a subscriber for its own thread (`set_default`). Whether an event
//! reaches it also depends on state every thread shares: the cached interest of each
//! callsite and the global maximum level. `tracing-core` computes both from its list of live
//! dispatchers, except while that list holds just one: then it asks the default of whichever
//! thread registers a callsite or rebuilds. A test thread with no subscriber of its own that
//! reaches a callsite first while a capture is the only live dispatcher caches that callsite
//! as never wanted, for every thread, and the capture misses its event. A rebuild from such
//! a thread does that to every callsite reached so far, and sets the maximum level from that
//! thread's default alone.
//!
//! So every capture first makes sure a keeper is registered: a dispatcher that wants
//! nothing, no thread's default, never dropped. With it the list never holds just one
//! dispatcher once a capture is installed, so the interest and the maximum level are always
//! computed from every live dispatcher, the capture's included, whichever thread asks.
//! Captures need no lock: two at once, on two threads, each see their own thread's events.

use std::io::Write;
use std::sync::OnceLock;

use tracing::Dispatch;
use tracing::Level;
use tracing::subscriber::{DefaultGuard, NoSubscriber};

/// Registers the keeper, once for the process: a dispatcher that is never dropped and no
/// thread's default. It answers that it wants no callsite, so it changes no event's fate;
/// being live, it keeps `tracing-core` from computing the shared interest from one thread's
/// default. Called before any subscriber of a test is made: [`capture`], and the tests of
/// `crate::warnings`, which install the agent's own subscriber.
pub(crate) fn keeper() {
    static KEEPER: OnceLock<Dispatch> = OnceLock::new();
    KEEPER.get_or_init(|| Dispatch::new(NoSubscriber::new()));
}

/// A subscriber installed for this thread; dropping it removes the subscriber.
pub(crate) struct Capture {
    _subscriber: DefaultGuard,
}

/// Installs, for this thread, a subscriber that writes every event at `level` or above to
/// `writer` in plain text. A thread this one starts that takes this thread's dispatcher (the
/// logs' uploader) writes there too. Installing it computes again the interest of every
/// callsite reached so far, from every live dispatcher, so a callsite another test cached
/// earlier sees it.
pub(crate) fn capture<W>(writer: W, level: Level) -> Capture
where
    W: Write + Clone + Send + Sync + 'static,
{
    keeper();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .with_max_level(level)
        .finish();
    Capture {
        _subscriber: tracing::subscriber::set_default(subscriber),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier, Mutex, PoisonError};

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

    /// Proves a capture sees an event at its level and not one below it, and nothing once
    /// it is dropped.
    #[test]
    fn a_capture_sees_events_at_its_level_and_none_below_or_after() {
        let sink = Sink::default();
        let held = capture(sink.clone(), Level::WARN);
        tracing::warn!("seen by the capture");
        tracing::info!("below its level");
        drop(held);
        tracing::warn!("after the capture");
        assert!(sink.clone().flush().is_ok());
        let text = sink.text();
        assert!(text.contains("seen by the capture"), "{text}");
        assert!(!text.contains("below its level"), "{text}");
        assert!(!text.contains("after the capture"), "{text}");
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
        assert_eq!(
            text.matches("from the shared callsite").count(),
            1,
            "{text}"
        );
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

    /// Proves two captures installed at once on two threads each see their own thread's
    /// events from one callsite, and not the other's. Both are installed before either
    /// thread logs (a barrier), so neither waits for the other. Catches captures that
    /// exclude each other, or that share one subscriber.
    #[test]
    fn two_captures_at_once_each_see_their_own_threads_events() {
        let both = Arc::new(Barrier::new(2));
        let run = |name: &'static str| {
            let both = Arc::clone(&both);
            std::thread::spawn(move || {
                let sink = Sink::default();
                let held = capture(sink.clone(), Level::WARN);
                both.wait();
                warn_from_one_callsite(name);
                drop(held);
                sink.text()
            })
        };
        let (first, second) = (run("first"), run("second"));
        let first = first.join().expect("the first thread ends");
        let second = second.join().expect("the second thread ends");
        assert!(first.contains("what=\"first\""), "{first}");
        assert!(!first.contains("what=\"second\""), "{first}");
        assert!(second.contains("what=\"second\""), "{second}");
        assert!(!second.contains("what=\"first\""), "{second}");
    }
}
