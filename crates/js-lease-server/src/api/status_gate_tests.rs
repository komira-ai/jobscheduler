//! A status route that is not served is a route that does not exist.

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
