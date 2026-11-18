//! The hello limit of the agent routes: a credential is held to its burst and interval before
//! the body of its hello is read.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Extension, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use js_agent_wire::refusal;
use js_registry::Credential;
use tracing::warn;

use super::Refused;
use crate::agents::Agents;

/// Holds a credential to its hello limit before the body is read: a 429 with the code
/// `rate-limited` and a `Retry-After` in whole seconds. The listener reads the body it
/// leaves and drops it before the 429 goes (`listener.rs`), so the connection does not
/// close on bytes it never read.
pub(super) async fn limit_hello(
    State(agents): State<Arc<Agents>>,
    Extension(who): Extension<Credential>,
    request: Request,
    next: Next,
) -> Response {
    match agents.limiter.admit(&who) {
        Ok(()) => next.run(request).await,
        Err(held) => {
            let seconds = retry_after(held.wait);
            if held.announce {
                warn!(
                    seconds,
                    "agent hello held to the rate limit; further ones are not logged until the next interval"
                );
            }
            let reply = Refused::new(StatusCode::TOO_MANY_REQUESTS, refusal::RATE_LIMITED);
            ([(header::RETRY_AFTER, seconds.to_string())], reply).into_response()
        }
    }
}

/// `wait` in whole seconds, rounded up, and at least one.
fn retry_after(wait: Duration) -> u64 {
    wait.as_millis()
        .div_ceil(1000)
        .max(1)
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A wait is told in whole seconds rounded up, never zero, whatever the fraction: a
    /// client told zero would retry at once, and one told too little retries into the
    /// limit again.
    #[test]
    fn a_wait_rounds_up_to_whole_seconds_and_is_never_zero() {
        let seconds = |millis| retry_after(Duration::from_millis(millis));
        assert_eq!(seconds(0), 1);
        assert_eq!(seconds(1), 1);
        assert_eq!(seconds(1000), 1);
        assert_eq!(seconds(1001), 2);
        assert_eq!(seconds(7000), 7);
        assert_eq!(retry_after(Duration::MAX), u64::MAX);
    }
}
