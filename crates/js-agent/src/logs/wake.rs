//! How the uploader's thread waits between steps: on the clock in production
//! ([`ClockWake`]), or until a test lets it take a step ([`ManualWake`]).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};

use js_logstore::Clock;

/// What the uploader's thread waits on between two steps.
pub trait Wake: Send + Sync {
    /// Returns at `until` on the uploader's clock, or earlier once [`Wake::notify`] was called
    /// since the last return.
    fn sleep_until(&self, until: SystemTime);
    /// Ends the current or the next sleep early: the job has ended.
    fn notify(&self);
    /// Says that the thread has ended (its work done, given up, or stopped by a panic).
    fn ended(&self);
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The production wake: sleeps for the time left until `until` by `clock`, unless notified.
pub struct ClockWake {
    clock: Arc<dyn Clock>,
    notified: Mutex<bool>,
    changed: Condvar,
}

impl ClockWake {
    /// A wake on `clock`.
    #[must_use]
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            clock,
            notified: Mutex::new(false),
            changed: Condvar::new(),
        }
    }
}

impl Wake for ClockWake {
    fn sleep_until(&self, until: SystemTime) {
        let left = until
            .duration_since(self.clock.now())
            .unwrap_or(Duration::ZERO);
        let notified = lock(&self.notified);
        let (mut notified, _) = self
            .changed
            .wait_timeout_while(notified, left, |notified| !*notified)
            .unwrap_or_else(PoisonError::into_inner);
        *notified = false;
    }

    fn notify(&self) {
        *lock(&self.notified) = true;
        self.changed.notify_all();
    }

    fn ended(&self) {}
}

/// What the thread of a [`ManualWake`] did with the step it was let take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Settled {
    /// It sleeps, waiting for the next step.
    Asleep,
    /// It has ended.
    Ended,
}

/// A wake for tests: the thread takes a step only when the test calls [`ManualWake::step`],
/// which returns once that step is over (the thread sleeps again, or has ended). Nothing waits
/// for a time: the test moves the uploader's clock itself.
///
/// The thread takes its first step as it starts; [`ManualWake::settled`] waits for its end.
/// After [`Settled::Ended`] the test calls neither again.
pub struct ManualWake {
    to_test: Mutex<Sender<Settled>>,
    from_thread: Mutex<Receiver<Settled>>,
    to_thread: Mutex<Sender<()>>,
    from_test: Mutex<Receiver<()>>,
    notified: AtomicU64,
}

impl ManualWake {
    /// A wake whose thread takes no step but its first until told.
    #[must_use]
    pub fn new() -> Arc<Self> {
        let (to_test, from_thread) = channel();
        let (to_thread, from_test) = channel();
        Arc::new(Self {
            to_test: Mutex::new(to_test),
            from_thread: Mutex::new(from_thread),
            to_thread: Mutex::new(to_thread),
            from_test: Mutex::new(from_test),
            notified: AtomicU64::new(0),
        })
    }

    /// Waits until the thread is done with the step it is taking.
    pub fn settled(&self) -> Settled {
        lock(&self.from_thread).recv().unwrap_or(Settled::Ended)
    }

    /// Lets the thread take one step and waits until it is done with it.
    pub fn step(&self) -> Settled {
        let _ = lock(&self.to_thread).send(());
        self.settled()
    }

    /// How many times the thread was notified of the job's end.
    #[must_use]
    pub fn notified(&self) -> u64 {
        self.notified.load(Ordering::SeqCst)
    }
}

impl Wake for ManualWake {
    fn sleep_until(&self, _until: SystemTime) {
        let _ = lock(&self.to_test).send(Settled::Asleep);
        let _ = lock(&self.from_test).recv();
    }

    fn notify(&self) {
        self.notified.fetch_add(1, Ordering::SeqCst);
    }

    fn ended(&self) {
        let _ = lock(&self.to_test).send(Settled::Ended);
    }
}
