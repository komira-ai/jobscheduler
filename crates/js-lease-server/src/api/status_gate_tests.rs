//! A status route that is not served is a route that does not exist.

use std::net::SocketAddr;

use axum::Extension;
use axum::body::{Body, to_bytes};
use axum::extract::{ConnectInfo, State};
use axum::http::{Request, StatusCode};
use axum::response::Response;
use tower::ServiceExt;

use super::whole_view_tests::Rig;
use super::{not_found, peer_may_read_status, pools, router, status_page};

/// The peer extension the listener sets on a connection's requests.
fn peer(text: &str) -> Option<Extension<ConnectInfo<SocketAddr>>> {
    let addr: SocketAddr = text.parse().unwrap();
    Some(Extension(ConnectInfo(addr)))
}

/// Asserts that with no status served both routes answer exactly as an unknown route
/// does: 404 and the same body. It catches a public bind that still serves the status.
#[tokio::test]
async fn an_unserved_status_answers_like_an_unknown_route() {
    let rig = Rig::new("status_gate").await;
    let unknown = not_found();
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
    let want = to_bytes(unknown.into_body(), 4096).await.unwrap();
    for reply in [
        pools(State(rig.shared.clone()), peer("127.0.0.1:1")).await,
        status_page(State(rig.shared.clone()), peer("127.0.0.1:1")).await,
    ] {
        assert_eq!(reply.status(), StatusCode::NOT_FOUND);
        assert_eq!(to_bytes(reply.into_body(), 4096).await.unwrap(), want);
    }
}

/// Asserts which peers may read the status pages: private addresses (loopback, the
/// private ranges, link-local, the tailnet range, unique-local IPv6, and an IPv4-mapped
/// IPv6 address whose IPv4 form is private) may; public IPv4 and IPv6 addresses (and a
/// mapped public one) may not; and no peer at all may not. It catches a gate that
/// classifies a mapped address by its IPv6 form, or that allows an unknown peer.
#[test]
fn the_peer_gate_allows_private_peers_only() {
    let allowed = [
        "127.0.0.1:1",
        "10.1.2.3:1",
        "172.16.0.9:1",
        "192.168.1.1:1",
        "169.254.1.1:1",
        "100.64.0.1:1",
        "100.127.255.255:1",
        "[::1]:1",
        "[fd7a:115c:a1e0::1]:1",
        "[fe80::1]:1",
        "[::ffff:10.0.0.1]:1",
        "[::ffff:127.0.0.1]:1",
        "[::ffff:100.64.0.1]:1",
    ];
    let refused = [
        "8.8.8.8:1",
        "203.0.113.7:1",
        "100.128.0.1:1",
        "172.32.0.1:1",
        "[2001:4860:4860::8888]:1",
        "[::ffff:8.8.8.8]:1",
        "[::ffff:203.0.113.7]:1",
        "[::ffff:100.128.0.1]:1",
    ];
    for text in allowed {
        assert!(peer_may_read_status(&peer(text)), "{text}");
    }
    for text in refused {
        assert!(!peer_may_read_status(&peer(text)), "{text}");
    }
    assert!(!peer_may_read_status(&None));
}

/// Sends `GET path` through the whole router, as the connection of `from` (or of no known
/// peer) would.
async fn get(rig: &Rig, path: &str, from: Option<&str>) -> Response {
    let mut request = Request::get(path).body(Body::empty()).unwrap();
    if let Some(Extension(info)) = from.and_then(peer) {
        request.extensions_mut().insert(info);
    }
    router(rig.shared.clone(), false)
        .oneshot(request)
        .await
        .unwrap()
}

/// Asserts that a status route served over the router answers a private peer with 200
/// and a public peer, a public IPv4-mapped peer and a request with no peer at all with
/// exactly the reply of an unknown route (404, same body). It catches a gate on the
/// listen address alone, a mapped address let through, and a missing peer let through.
#[tokio::test]
async fn a_status_route_answers_only_a_private_peer() {
    let rig = Rig::serving_status("status_peer").await;
    let unknown = not_found();
    let want = to_bytes(unknown.into_body(), 4096).await.unwrap();
    for path in ["/v1/pools", "/status"] {
        for from in ["10.0.0.5:4000", "[::ffff:10.0.0.5]:4000", "100.64.1.1:4000"] {
            let reply = get(&rig, path, Some(from)).await;
            assert_eq!(reply.status(), StatusCode::OK, "{path} from {from}");
        }
        for from in [
            Some("203.0.113.7:4000"),
            Some("[2001:4860:4860::8888]:4000"),
            Some("[::ffff:8.8.8.8]:4000"),
            None,
        ] {
            let reply = get(&rig, path, from).await;
            assert_eq!(
                reply.status(),
                StatusCode::NOT_FOUND,
                "{path} from {from:?}"
            );
            let body = to_bytes(reply.into_body(), 4096).await.unwrap();
            assert_eq!(body, want, "{path} from {from:?}");
        }
    }
}

/// Asserts what a launch is planned for: the pool floor with no registry and with nothing
/// waiting, and the first request on the wait list once one waits (a whole-node request
/// that finds the only host taken), named by its id and with its own demand. It catches a
/// plan made for the floor while a request waits.
#[tokio::test]
async fn a_launch_is_planned_for_the_head_of_the_wait_list_else_the_floor() {
    use js_elastic::Demand;
    use js_types::Resources;

    use crate::status::wanted;

    let floor = Demand {
        resources: Resources::new(1000, 1 << 30, 0, None).unwrap(),
        disk_bytes: None,
        os: None,
        arch: None,
        exec: None,
    };
    let mut rig = Rig::new("status_wanted").await;
    for registry in [None, Some(&rig.registry)] {
        let asked = wanted(registry, &floor);
        assert_eq!(
            (asked.request.as_str(), asked.demand),
            ("floor", floor.clone())
        );
    }
    rig.lease().await;
    rig.ask("r1");
    let asked = wanted(Some(&rig.registry), &floor);
    assert_eq!(asked.request, "r1");
    assert_ne!(asked.demand, floor);
}
