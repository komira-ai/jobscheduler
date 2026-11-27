//! The routing codes of the routes that remain: health and the host listing; and that the
//! routes of the join tokens are gone.

use serde_json::json;

use crate::agents_support::{listed, start_agents};
use crate::support::{Harness, request};

/// Asserts that the join-token routes are gone: `POST` and `DELETE` of `/v1/agent/join-tokens`
/// with the client token are answered 404 `not-found`, as any path the server does not have
/// is, and the host listing is unchanged. It catches a route of the removed tokens that is
/// still served.
#[tokio::test]
async fn the_join_token_routes_are_not_routes() {
    let h = start_agents("join_token_routes_gone").await;
    let body = json!({ "provider": "lambdalabs", "instance_id": "i-1" });
    for method in ["POST", "DELETE"] {
        let reply = h.call(method, "/v1/agent/join-tokens", Some(&body)).await;
        assert_eq!(reply.status, 404, "{method}: {}", reply.body);
        assert_eq!(reply.json(), json!({ "error": "not-found" }), "{method}");
    }
    let listing = h.call("GET", "/v1/hosts", None).await;
    assert_eq!(listing.status, 200);
    assert!(listed(&listing.json()).is_empty());
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
