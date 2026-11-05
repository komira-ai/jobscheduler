//! Awaiting a future on the calling thread, for code with no async runtime of its own: the
//! conformance cases and this crate's tests, which await [`crate::CapacityProvider::catalog`].
//!
//! The thread parks while the future is pending and is unparked by the future's waker. A
//! provider whose catalog runs on an async runtime's blocking pool needs that runtime
//! entered on the calling thread; nothing here starts one.

use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, Thread};

/// Wakes a parked thread.
struct Unpark(Thread);

impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

/// Awaits `future` on this thread and returns its output.
pub fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let waker = Waker::from(Arc::new(Unpark(thread::current())));
    let mut context = Context::from_waker(&waker);
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return output;
        }
        thread::park();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A future that is pending `left` times, waking itself each time (by a clone of the
    /// waker, from another thread), and then ready with how often it was polled.
    struct Later {
        left: u32,
        polls: u32,
    }

    impl Future for Later {
        type Output = u32;

        fn poll(mut self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<u32> {
            self.polls += 1;
            match self.left {
                0 => Poll::Ready(self.polls),
                _ => {
                    self.left -= 1;
                    let waker = cx.waker().clone();
                    thread::spawn(move || waker.wake()).join().unwrap();
                    Poll::Pending
                }
            }
        }
    }

    /// Asserts that a ready future is polled once, and that a future pending twice is
    /// polled again after each wake and its output returned on the third poll. Catches a
    /// wait that returns before the future is ready, or never wakes.
    #[test]
    fn a_pending_future_is_polled_again_after_its_wake() {
        assert_eq!(block_on(Later { left: 0, polls: 0 }), 1);
        assert_eq!(block_on(Later { left: 2, polls: 0 }), 3);
    }
}
