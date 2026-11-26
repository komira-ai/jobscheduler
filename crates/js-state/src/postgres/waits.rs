//! The time a call spent getting connections of a pool: [`checkout_waits`] runs a call on this
//! thread under a tally, and each checkout of a pool the call makes on the thread while the
//! tally is open (an idle connection taken, a new one dialled, or the wait for one to come
//! back) adds its time to it, by the tally's clock. A checkout outside a tally reads no clock.
//!
//! The tally is the calling thread's own, so a call is told only of the checkouts it made
//! itself: a checkout of another thread at the same time adds nothing to it. A tally opened
//! inside another is told the checkouts made inside it, and the outer one is not; the outer one
//! is open again once the inner one ends.

use std::cell::RefCell;
use std::sync::Arc;
use std::time::Duration;

use crate::clock::Clock;

/// An open tally: the clock it reads and what its checkouts have taken so far.
struct Tally {
    clock: Arc<dyn Clock>,
    waited: Duration,
}

thread_local! {
    /// The tally open on this thread, if any.
    static TALLY: RefCell<Option<Tally>> = const { RefCell::new(None) };
}

/// Puts the tally that was open before back when a tally ends, even when its call panicked.
struct Reopen(Option<Tally>);

impl Drop for Reopen {
    fn drop(&mut self) {
        let outer = self.0.take();
        TALLY.with(|tally| *tally.borrow_mut() = outer);
    }
}

/// Runs `work` under a tally read by `clock`: what `work` returned, and the time the checkouts
/// of a pool it made on this thread took, added up (zero when it made none).
pub fn checkout_waits<T>(clock: Arc<dyn Clock>, work: impl FnOnce() -> T) -> (T, Duration) {
    let opened = Tally {
        clock,
        waited: Duration::ZERO,
    };
    let reopen = Reopen(TALLY.with(|tally| tally.replace(Some(opened))));
    let done = work();
    let waited = TALLY.with(|tally| tally.borrow().as_ref().map(|open| open.waited));
    drop(reopen);
    (done, waited.unwrap_or_default())
}

/// Runs `checkout`, adding the time it took to the tally open on this thread; with none open,
/// only runs it.
pub(super) fn timed<T>(checkout: impl FnOnce() -> T) -> T {
    let clock = TALLY.with(|tally| tally.borrow().as_ref().map(|open| Arc::clone(&open.clock)));
    let Some(clock) = clock else {
        return checkout();
    };
    let began = clock.now();
    let done = checkout();
    let took = clock.now().since(began);
    TALLY.with(|tally| {
        let mut tally = tally.borrow_mut();
        tally
            .iter_mut()
            .for_each(|open| open.waited = open.waited.saturating_add(took));
    });
    done
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use super::{checkout_waits, timed};
    use crate::clock::{Clock, StoreTime};

    /// A clock that moves `step` milliseconds on each reading.
    #[derive(Debug)]
    struct Stepping {
        step: u64,
        read: AtomicU64,
    }

    impl Stepping {
        fn by(step: u64) -> Arc<Stepping> {
            Arc::new(Stepping {
                step,
                read: AtomicU64::new(0),
            })
        }
    }

    impl Clock for Stepping {
        fn now(&self) -> StoreTime {
            let millis = self.read.fetch_add(self.step, Ordering::Relaxed);
            StoreTime::from_duration(Duration::from_millis(millis))
        }
    }

    // Catches a tally that keeps only the last checkout, and one that reads its clock outside
    // a checkout (the call's own work would then count as a wait).
    #[test]
    fn each_checkout_of_a_call_is_added_and_the_rest_of_the_call_is_not() {
        let clock = Stepping::by(3);
        let (done, waited) = checkout_waits(clock.clone(), || {
            let first = timed(|| 1);
            let second = timed(|| 2);
            first + second
        });
        assert_eq!(done, 3);
        assert_eq!(waited, Duration::from_millis(6));
        assert_eq!(clock.read.load(Ordering::Relaxed), 12);
    }

    // Catches a tally told nothing being anything but zero, and a checkout outside a tally that
    // reads a clock or is added to the next tally.
    #[test]
    fn a_call_with_no_checkout_waited_nothing_and_a_checkout_outside_a_tally_counts_nowhere() {
        let clock = Stepping::by(3);
        assert_eq!(timed(|| "outside"), "outside");
        let (done, waited) = checkout_waits(clock.clone(), || "no checkout");
        assert_eq!((done, waited), ("no checkout", Duration::ZERO));
        assert_eq!(clock.read.load(Ordering::Relaxed), 0);
        assert_eq!(timed(|| "after"), "after");
        assert_eq!(clock.read.load(Ordering::Relaxed), 0);
    }

    // Catches an inner tally whose checkouts reach the outer one too, and an inner tally that
    // leaves the outer one closed behind it.
    #[test]
    fn a_tally_inside_another_is_told_its_own_checkouts_and_the_outer_one_reopens() {
        let (outer_clock, inner_clock) = (Stepping::by(5), Stepping::by(2));
        let (inner, outer) = checkout_waits(outer_clock, || {
            timed(|| ());
            let inner = checkout_waits(inner_clock, || timed(|| ())).1;
            timed(|| ());
            inner
        });
        assert_eq!(inner, Duration::from_millis(2));
        assert_eq!(outer, Duration::from_millis(10));
    }

    // Catches a tally shared between threads: a checkout of another thread at the same time
    // adds nothing to this thread's.
    #[test]
    fn a_checkout_on_another_thread_is_not_added() {
        let clock = Stepping::by(4);
        let ((), waited) = checkout_waits(clock, || {
            std::thread::scope(|scope| {
                scope.spawn(|| timed(|| ()));
            });
        });
        assert_eq!(waited, Duration::ZERO);
    }

    // Catches a tally left open after its call panicked, which would add every later checkout
    // of the thread to it.
    #[test]
    fn a_tally_whose_call_panicked_is_closed() {
        let clock = Stepping::by(1);
        let panicked = std::panic::catch_unwind(|| {
            checkout_waits(clock.clone(), || panic!("the call fails"));
        });
        assert!(panicked.is_err());
        let read = clock.read.load(Ordering::Relaxed);
        timed(|| ());
        assert_eq!(clock.read.load(Ordering::Relaxed), read);
    }
}
