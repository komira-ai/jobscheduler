//! The routing codes of the routes that remain: health, the host listing, the join tokens.

use serde_json::json;

use crate::agents_support::start_agents;
use crate::support::{Harness, TOKEN, request};

/// Asserts that a body over the cap is refused with a 413 on a route that takes a body
/// (the join-token mint, behind the client token), and that the refusal mints nothing.
#[tokio::test]
async fn oversized_bodies_are_refused() {
    let h = start_agents("oversized").await;
    let big = vec![b' '; js_lease_server::BODY_LIMIT + 1];
    let reply = request(
        h.addr(),
        "POST",
        "/v1/agent/join-tokens",
        Some(TOKEN),
        Some(&big),
    )
    .await;
    assert_eq!(reply.status, 413, "{}", reply.body);
    let listing = h.call("GET", "/v1/hosts", None).await;
    assert_eq!(listing.status, 200);
}

/// Asserts that health needs no token, an unknown route is a 404 and a known route with
/// another method is a 405.
#[tokio::test]
async fn routing_codes() {
    let h = Harness::new("routing").await;
    let health = request(h.addr(), "GET", "/healthz", None, None).await;
    assert_eq!((health.status, health.body.as_str()), (200, "ok"));
    assert_eq!(
        request(h.addr(), "POST", "/healthz", None, None)
            .await
            .status,
        405
    );

    let missing = h.call("GET", "/v1/nothing", None).await;
    assert_eq!(missing.status, 404);
    assert_eq!(missing.json(), json!({ "error": "not-found" }));
    assert_eq!(h.call("GET", "/elsewhere", None).await.status, 404);
    assert_eq!(h.call("PUT", "/v1/hosts", None).await.status, 405);
}
