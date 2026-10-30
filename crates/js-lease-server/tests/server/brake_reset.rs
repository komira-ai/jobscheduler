//! The operator's reset of a pool's failure brake through a whole server: a tripped pool is
//! shown in plan with an alert, the reset needs the client token and a pool the server has,
//! it is applied at the next tick and written, and asking twice is harmless. Time is the
//! test's clock and the ticks are stepped.

use std::collections::BTreeMap;

use js_lease_server::RecordsFile;
use js_scaler::{Brake, Millis, Records};
use serde_json::{Value, json};

use crate::scaler_support::{account, records_path, start, text};
use crate::support::{Harness, Scratch, T0, request};

/// A server whose pool `burst` was tripped by an earlier run: the records on disk hold the
/// trip, and the checks have passed.
async fn tripped(name: &str) -> Harness {
    let scratch = Scratch::new(name);
    account(&scratch);
    let first = text(&scratch, "live", "initialise");
    let h = start(scratch, &first).await;
    let Harness {
        running, scratch, ..
    } = h;
    running.shutdown().await.unwrap();
    let mut brake = BTreeMap::new();
    let trip = Brake {
        failures: 3,
        tripped_at: Some(Millis::new(T0)),
    };
    brake.insert("burst".to_owned(), trip);
    let records = Records {
        brake,
        ..Default::default()
    };
    RecordsFile::new(records_path(&scratch))
        .save(&records)
        .unwrap();
    let again = text(&scratch, "live", "resume");
    start(scratch, &again).await
}

/// The one pool of `GET /v1/pools`.
async fn pool(h: &Harness) -> Value {
    let reply = h.call("GET", "/v1/pools", None).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    reply.json()["pools"][0].clone()
}

/// Asserts what a restarted server shows of a tripped pool: it is in plan, `blocked` says
/// `brake`, and one alert names the pool and the way out. It catches a trip the records hold
/// and the status does not show, which an operator would only find by a request that never
/// gets a machine.
#[tokio::test]
async fn a_tripped_pool_is_in_plan_with_an_alert() {
    let h = tripped("brake_status").await;
    h.ticks.step().await;
    let view = pool(&h).await;
    assert_eq!(view["launch"], "plan");
    assert_eq!(view["blocked"], "brake");
    let alerts = view["alerts"].as_array().unwrap();
    assert_eq!(alerts.len(), 1, "{view}");
    let said = alerts[0].as_str().unwrap();
    assert!(said.contains("failure brake"), "{said}");
    assert!(said.contains("/v1/pools/burst/brake/reset"), "{said}");
}

/// Asserts the reset route: no token or a wrong one is a 401, a pool the server does not
/// have is a 404 and changes nothing, the right pool is a 202; at the next tick the pool is
/// live again with no alert and the records on disk hold no brake, and a second request is
/// also a 202 and leaves it so. It catches a route outside the token, one that resets
/// whatever name it is given, and a reset that is not written.
#[tokio::test]
async fn the_reset_needs_the_token_and_a_known_pool_and_is_idempotent() {
    let h = tripped("brake_reset").await;
    h.ticks.step().await;
    let path = "/v1/pools/burst/brake/reset";
    for token in [None, Some("nope")] {
        let reply = request(h.addr(), "POST", path, token, None).await;
        assert_eq!(reply.status, 401, "{token:?}");
    }
    let other = h
        .call("POST", "/v1/pools/elsewhere/brake/reset", None)
        .await;
    assert_eq!(other.status, 404);
    h.ticks.step().await;
    assert_eq!(pool(&h).await["blocked"], "brake");

    let reply = h.call("POST", path, None).await;
    assert_eq!(reply.status, 202, "{}", reply.body);
    assert_eq!(reply.json(), json!({"pool": "burst", "reset": "requested"}));
    h.ticks.step().await;
    let view = pool(&h).await;
    assert_eq!(view["launch"], "live");
    assert_ne!(view["blocked"], "brake");
    assert_eq!(view["alerts"], json!([]));
    let on_disk = RecordsFile::new(records_path(&h.scratch)).load().unwrap();
    assert!(on_disk.brake.is_empty());

    let again = h.call("POST", path, None).await;
    assert_eq!(again.status, 202);
    h.ticks.step().await;
    let view = pool(&h).await;
    assert_eq!(view["launch"], "live");
    assert_eq!(view["alerts"], json!([]));
}
