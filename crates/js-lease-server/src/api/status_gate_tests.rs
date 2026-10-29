//! A status route that is not served is a route that does not exist.

use std::sync::Arc;

use axum::body::to_bytes;
use axum::extract::State;
use axum::http::StatusCode;

use super::whole_view_tests::Rig;
use super::{not_found, pools, status_page};

/// Asserts that with no status served both routes answer exactly as an unknown route
/// does: 404 and the same body. It catches a public bind that still serves the status.
#[tokio::test]
async fn an_unserved_status_answers_like_an_unknown_route() {
    let rig = Rig::new("status_gate").await;
    let unknown = not_found();
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
    let want = to_bytes(unknown.into_body(), 4096).await.unwrap();
    for reply in [
        pools(State(rig.shared.clone())).await,
        status_page(State(rig.shared.clone())).await,
    ] {
        assert_eq!(reply.status(), StatusCode::NOT_FOUND);
        assert_eq!(to_bytes(reply.into_body(), 4096).await.unwrap(), want);
    }
}
