//! Small helpers shared by the modules.

use std::future::pending;

/// `value` when there is one; otherwise a future that never completes. A channel whose
/// sender is gone has no more to say, and the caller waits on its other events (a
/// shutdown) instead of ending the server or spinning.
pub(crate) async fn or_pending<T>(value: Option<T>) -> T {
    match value {
        Some(value) => value,
        None => pending().await,
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::task::{Context, Poll, Waker};

    use super::*;

    /// Asserts that a value is passed through and that no value never completes.
    #[test]
    fn passes_a_value_and_waits_for_none() {
        let mut cx = Context::from_waker(Waker::noop());
        let mut ready = std::pin::pin!(or_pending(Some(3u8)));
        assert_eq!(ready.as_mut().poll(&mut cx), Poll::Ready(3u8));
        let mut waiting = std::pin::pin!(or_pending(None::<u8>));
        assert_eq!(waiting.as_mut().poll(&mut cx), Poll::Pending);
    }
}
