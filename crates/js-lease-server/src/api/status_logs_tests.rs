//! The counters of the host credentials of live logs on the two status routes, asked through
//! the router as a private peer asks: `GET /v1/pools` carries them as `logs`, and `GET /status`
//! shows them in its "Live logs" table. A server without a sink shows neither.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use js_logmint::fake::FakeMinter;
use js_logmint::{MintError, Minter};
use js_logstore::fake::FakeStore;
use js_logstore::{Endpoint, Unavailable};
use js_types::{ManualClock, Timestamp};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::rig::Rig;
use super::router;
use crate::agent_api::job_rig::{LOG_BUCKET, LOG_ENDPOINT, LOG_REGION, START_MS, grants_over};
use crate::clock::SharedClock;
use crate::logs::{LogGrants, StoreClock};
use crate::util::blocking;

/// The body of `GET path`, asked through the whole router by a private peer, which is
/// answered 200.
async fn body_of(rig: &Rig, path: &str) -> String {
    let mut request = Request::get(path).body(Body::empty()).unwrap();
    let peer: SocketAddr = "10.0.0.5:4000".parse().unwrap();
    request.extensions_mut().insert(ConnectInfo(peer));
    let reply = router(rig.shared.clone(), false)
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(reply.status(), StatusCode::OK, "{path}");
    let body = to_bytes(reply.into_body(), 1 << 20).await.unwrap();
    String::from_utf8(body.to_vec()).unwrap()
}

/// Grants over js-logmint's fake minter in which one host holds a credential and another's
/// only mint failed; the mints have ended.
async fn grants_with_one_held_and_one_failed() -> Arc<LogGrants> {
    let clock = SharedClock::new(ManualClock::new(Timestamp::from_millis(START_MS)));
    let endpoint = Endpoint::parse(LOG_ENDPOINT, LOG_REGION, LOG_BUCKET, true).unwrap();
    let store = FakeStore::new(endpoint.clone(), Arc::new(StoreClock(clock.clone())));
    let minter = Arc::new(FakeMinter::new(store));
    let grants = grants_over(endpoint, Arc::clone(&minter) as Arc<dyn Minter>, &clock);
    let settle = |grants: &Arc<LogGrants>| {
        let grants = Arc::clone(grants);
        blocking(move || grants.settle())
    };
    let unavailable = Unavailable::Status {
        status: 503,
        code: "SlowDown".to_owned(),
    };
    minter.fail_next(1, MintError::Unavailable(unavailable));
    grants.prepare("host-failing");
    settle(&grants).await;
    grants.prepare("host-held");
    settle(&grants).await;
    assert_eq!(
        (minter.calls("host-failing"), minter.calls("host-held")),
        (1, 1)
    );
    grants
}

/// With grants in which one mint succeeded and one failed, `GET /v1/pools` carries
/// `logs` as exactly those four counters and `GET /status` shows each in its row of the
/// "Live logs" table; a server without a sink carries no `logs` and shows no such table.
/// It catches counters left off the routes the server mounts, a page that shows them
/// under the wrong labels, and a server without a sink that shows zeros.
#[tokio::test]
async fn both_status_routes_show_the_live_log_counters_and_only_with_a_sink() {
    let grants = grants_with_one_held_and_one_failed().await;
    let rig = Rig::serving_status_with_logs("status_logs", grants).await;
    let pools: Value = serde_json::from_str(&body_of(&rig, "/v1/pools").await).unwrap();
    assert_eq!(
        pools["logs"],
        json!({
            "minted": 1,
            "failed": 1,
            "hosts_with_credential": 1,
            "hosts_failing": 1,
        })
    );
    let page = body_of(&rig, "/status").await;
    let table = "<h2>Live logs</h2>\n<table>\n\
        <tr><th align=\"left\">credentials minted</th><td>1</td></tr>\n\
        <tr><th align=\"left\">mints failed</th><td>1</td></tr>\n\
        <tr><th align=\"left\">hosts with a credential</th><td>1</td></tr>\n\
        <tr><th align=\"left\">hosts whose last mint failed</th><td>1</td></tr>\n\
        </table>\n";
    assert!(page.contains(table), "{page}");

    let bare = Rig::serving_status("status_no_logs").await;
    let pools: Value = serde_json::from_str(&body_of(&bare, "/v1/pools").await).unwrap();
    assert_eq!(pools.get("logs"), None, "{pools}");
    let page = body_of(&bare, "/status").await;
    assert!(!page.contains("Live logs"), "{page}");
}
