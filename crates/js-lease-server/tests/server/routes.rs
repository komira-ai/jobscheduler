//! Every route's success and error codes.

use serde_json::{Value, json};

use crate::support::{Harness, T0, first_labels, ids, lease_body, request};

fn with(mut body: Value, key: &str, value: Value) -> Value {
    body[key] = value;
    body
}

/// Asserts that a lease request is granted with 201, an id of (term, sequence), the
/// first free host and an expiry of now plus the default ttl, and that the lease then
/// shows in the list with its labels.
#[tokio::test]
async fn create_grants_a_lease() {
    let h = Harness::new("create").await;
    let reply = h.call("POST", "/v1/leases", Some(&lease_body())).await;
    assert_eq!(reply.status, 201, "{}", reply.body);
    let body = reply.json();
    assert_eq!(body["id"], "1-1");
    assert_eq!(body["host"], "gpu-01");
    assert_eq!(body["expires_at_ms"], T0 + 600_000);

    let listed = h.call("GET", "/v1/leases", None).await;
    assert_eq!(listed.status, 200);
    assert_eq!(ids(&listed), vec!["1-1"]);
    let lease = &listed.json()["leases"][0];
    assert_eq!(lease["state"], "held");
    assert_eq!(lease["host"], "gpu-01");
    assert_eq!(lease["acquired_at_ms"], T0);
    assert_eq!(lease["ends_at_ms"], T0 + 7_200_000);
    assert_eq!(first_labels(&listed)["team"], "infra");
}

/// Asserts that a ttl the client names is used, a validation run becomes a label, and a
/// ttl above the maximum is cut to it.
#[tokio::test]
async fn create_honours_ttl_and_validation_run() {
    let h = Harness::new("create_ttl").await;
    let body = with(lease_body(), "ttl_seconds", json!(30));
    let body = with(body, "validation_run", json!("val-7"));
    let reply = h.call("POST", "/v1/leases", Some(&body)).await;
    assert_eq!(reply.json()["expires_at_ms"], T0 + 30_000);
    let listed = h.call("GET", "/v1/leases", None).await;
    let labels = first_labels(&listed);
    assert!(labels.values().any(|value| value == "val-7"), "{labels:?}");

    let long = with(lease_body(), "ttl_seconds", json!(999_999));
    let reply = h.call("POST", "/v1/leases", Some(&long)).await;
    assert_eq!(reply.json()["expires_at_ms"], T0 + 3_600_000);
}

/// Asserts that a request the provider cannot place is a 409 naming the code: no host
/// is free, or none could ever fit.
#[tokio::test]
async fn create_refusals_are_409() {
    let h = Harness::new("create_409").await;
    for _ in 0..2 {
        assert_eq!(
            h.call("POST", "/v1/leases", Some(&lease_body()))
                .await
                .status,
            201
        );
    }
    let full = h.call("POST", "/v1/leases", Some(&lease_body())).await;
    assert_eq!(full.status, 409);
    assert_eq!(full.json(), json!({ "error": "no-capacity" }));

    let big = json!({ "gpus": 16, "gpu_class": "nvidia-a100" });
    let body = with(lease_body(), "resources", big);
    let reply = h.call("POST", "/v1/leases", Some(&body)).await;
    assert_eq!(reply.status, 409);
    assert_eq!(reply.json(), json!({ "error": "no-fit" }));
}

/// Asserts that a request breaking the start contract or its class is a 422 naming the
/// rule, and records nothing.
#[tokio::test]
async fn create_contract_refusals_are_422() {
    let h = Harness::new("create_422").await;
    let cases = [
        (
            with(lease_body(), "max_wall_seconds", json!(0)),
            "wall-time-not-positive",
        ),
        (
            with(lease_body(), "labels", json!({ "jobscheduler.dev/x": "1" })),
            "reserved-label",
        ),
        (
            with(lease_body(), "labels", json!({ "": "1" })),
            "invalid-label",
        ),
        (with(lease_body(), "class", json!("batch")), "unknown-class"),
        (
            with(lease_body(), "resources", json!({ "cpu_millis": 1 })),
            "below-class-minimum",
        ),
    ];
    for (body, code) in cases {
        let reply = h.call("POST", "/v1/leases", Some(&body)).await;
        assert_eq!(reply.status, 422, "{code}: {}", reply.body);
        assert_eq!(reply.json(), json!({ "error": code }));
    }
    assert!(ids(&h.call("GET", "/v1/leases", None).await).is_empty());
}

/// Asserts that a body that is not the JSON the route takes is a 400 with a short
/// reason, for each way it can be wrong.
#[tokio::test]
async fn create_bad_bodies_are_400() {
    let h = Harness::new("create_400").await;
    let mut cases = vec![
        with(lease_body(), "class", json!("Not A Class")),
        with(lease_body(), "resources", json!({ "gpus": 1 })),
        with(lease_body(), "ttl_seconds", json!(0)),
        with(lease_body(), "surprise", json!(1)),
        with(lease_body(), "max_wall_seconds", json!("long")),
    ];
    cases.push(json!({}));
    for body in cases {
        let reply = h.call("POST", "/v1/leases", Some(&body)).await;
        assert_eq!(reply.status, 400, "{body}: {}", reply.body);
        let error = reply.json();
        assert_eq!(error["error"], "bad-request");
        assert!(error["reason"].as_str().unwrap().len() < 300);
    }
    let raw = request(
        h.addr(),
        "POST",
        "/v1/leases",
        Some(crate::support::TOKEN),
        Some(b"{nope"),
    )
    .await;
    assert_eq!(raw.status, 400);
}

/// Asserts that a body over the cap is refused with a 413 and never reaches the table.
#[tokio::test]
async fn oversized_bodies_are_refused() {
    let h = Harness::new("oversized").await;
    let big = vec![b' '; js_lease_server::BODY_LIMIT + 1];
    let reply = request(
        h.addr(),
        "POST",
        "/v1/leases",
        Some(crate::support::TOKEN),
        Some(&big),
    )
    .await;
    assert_eq!(reply.status, 413);
    assert!(ids(&h.call("GET", "/v1/leases", None).await).is_empty());
}

/// Asserts that a renew extends from now, never shortens, takes an empty body as the
/// default ttl, and refuses a bad ttl, an unknown or malformed id and an expired lease.
#[tokio::test]
async fn renew_codes() {
    let h = Harness::new("renew").await;
    h.call("POST", "/v1/leases", Some(&lease_body())).await;
    h.clock.advance(std::time::Duration::from_secs(100));

    let reply = h.call("POST", "/v1/leases/1-1/renew", None).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(
        reply.json(),
        json!({ "expires_at_ms": T0 + 100_000 + 600_000 })
    );

    let short = h
        .call(
            "POST",
            "/v1/leases/1-1/renew",
            Some(&json!({ "ttl_seconds": 5 })),
        )
        .await;
    assert_eq!(short.json(), json!({ "expires_at_ms": T0 + 700_000 }));

    let zero = h
        .call(
            "POST",
            "/v1/leases/1-1/renew",
            Some(&json!({ "ttl_seconds": 0 })),
        )
        .await;
    assert_eq!(zero.status, 400);
    let junk = h
        .call("POST", "/v1/leases/1-1/renew", Some(&json!({ "other": 1 })))
        .await;
    assert_eq!(junk.status, 400);

    for id in ["9-9", "abc", "1-01", "+1-1", "1-", "-1"] {
        let reply = h
            .call("POST", &format!("/v1/leases/{id}/renew"), None)
            .await;
        assert_eq!(reply.status, 404, "{id}");
        assert_eq!(reply.json(), json!({ "error": "unknown-lease" }));
    }

    h.clock.advance(std::time::Duration::from_secs(1000));
    let late = h.call("POST", "/v1/leases/1-1/renew", None).await;
    assert_eq!(late.status, 409);
    assert_eq!(late.json(), json!({ "error": "expired" }));
}

/// Asserts that a release frees the lease with a 200, a second release of it is a 404,
/// and the host is free for the next lease.
#[tokio::test]
async fn release_codes() {
    let h = Harness::new("release").await;
    h.call("POST", "/v1/leases", Some(&lease_body())).await;
    let freed = h.call("DELETE", "/v1/leases/1-1", None).await;
    assert_eq!(freed.status, 200);
    assert_eq!(freed.json(), json!({ "state": "freed" }));
    let again = h.call("DELETE", "/v1/leases/1-1", None).await;
    assert_eq!(again.status, 404);
    assert_eq!(h.call("DELETE", "/v1/leases/nope", None).await.status, 404);
    let next = h.call("POST", "/v1/leases", Some(&lease_body())).await;
    assert_eq!(next.json()["host"], "gpu-01");
}

/// Asserts that an empty table lists as an empty array under `leases`.
#[tokio::test]
async fn list_when_empty() {
    let h = Harness::new("list_empty").await;
    let reply = h.call("GET", "/v1/leases", None).await;
    assert_eq!(reply.status, 200);
    assert_eq!(reply.json(), json!({ "leases": [] }));
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
    assert_eq!(h.call("PUT", "/v1/leases", None).await.status, 405);
    assert_eq!(h.call("GET", "/v1/leases/1-1", None).await.status, 405);
    assert_eq!(
        h.call("GET", "/v1/leases/1-1/renew", None).await.status,
        405
    );
}

/// Asserts that a lease file that cannot be written fails the request with a 500 that
/// does not name the path: before the start (create), and after a change (renew,
/// release). The write is made to fail by putting a directory where the temporary file
/// goes.
#[tokio::test]
async fn unwritable_state_is_500() {
    let h = Harness::new("unwritable").await;
    h.call("POST", "/v1/leases", Some(&lease_body())).await;
    std::fs::create_dir(h.scratch.path("leases.toml.tmp")).unwrap();

    let cases = [
        ("POST", "/v1/leases", Some(lease_body())),
        ("POST", "/v1/leases/1-1/renew", None),
        ("DELETE", "/v1/leases/1-1", None),
    ];
    for (method, path, body) in cases {
        let reply = h.call(method, path, body.as_ref()).await;
        assert_eq!(reply.status, 500, "{method} {path}");
        assert_eq!(reply.json(), json!({ "error": "state-write-failed" }));
    }
}
