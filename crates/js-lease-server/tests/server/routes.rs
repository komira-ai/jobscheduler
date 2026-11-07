//! Every route's success and error codes.

use serde_json::{Value, json};

use crate::support::{Harness, T0, first_labels, ids, lease_body, request};

fn with(mut body: Value, key: &str, value: Value) -> Value {
    body[key] = value;
    body
}

/// Asserts that a lease request is granted with 201, an id of (term, sequence), the
/// first free host and its wall end (`ends_at_ms`) of now plus the wall time asked for,
/// and that the lease then shows in the list with its labels and no `expires_at_ms`.
#[tokio::test]
async fn create_grants_a_lease() {
    let h = Harness::new("create").await;
    let reply = h.call("POST", "/v1/leases", Some(&lease_body())).await;
    assert_eq!(reply.status, 201, "{}", reply.body);
    let body = reply.json();
    assert_eq!(body["id"], "1-1");
    assert_eq!(body["host"], "gpu-01");
    assert_eq!(body["ends_at_ms"], T0 + 7_200_000);
    assert_eq!(body.as_object().unwrap().len(), 3, "{body}");

    let listed = h.call("GET", "/v1/leases", None).await;
    assert_eq!(listed.status, 200);
    assert_eq!(ids(&listed), vec!["1-1"]);
    let lease = &listed.json()["leases"][0];
    assert_eq!(lease["state"], "held");
    assert_eq!(lease["host"], "gpu-01");
    assert_eq!(lease["acquired_at_ms"], T0);
    assert_eq!(lease["ends_at_ms"], T0 + 7_200_000);
    assert!(lease.get("expires_at_ms").is_none(), "{lease}");
    assert_eq!(first_labels(&listed)["team"], "infra");
}

/// Asserts that a validation run becomes a label on the lease.
#[tokio::test]
async fn create_records_a_validation_run() {
    let h = Harness::new("create_validation").await;
    let body = with(lease_body(), "validation_run", json!("val-7"));
    let reply = h.call("POST", "/v1/leases", Some(&body)).await;
    assert_eq!(reply.status, 201, "{}", reply.body);
    let listed = h.call("GET", "/v1/leases", None).await;
    let labels = first_labels(&listed);
    assert!(labels.values().any(|value| value == "val-7"), "{labels:?}");
}

/// Asserts that a request the provider cannot place is a 409 naming the code: no host
/// is free (`no-capacity`), or no host could ever hold it, which admission refuses
/// (`no-capable-host`).
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
    assert_eq!(reply.json(), json!({ "error": "no-capable-host" }));
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

/// Asserts that the renew route is gone: with a lease held, `POST /v1/leases/{id}/renew`
/// is the router's ordinary answer to a path it does not have (404 `not-found`, the same
/// status and body as `/v1/nothing`), whatever the id, with or without a body, and the
/// lease is as it was (still held, the same `ends_at_ms`). It catches a renew route that
/// still answers, and one that changes the lease while refusing.
#[tokio::test]
async fn the_renew_route_is_gone() {
    let h = Harness::new("renew_gone").await;
    h.call("POST", "/v1/leases", Some(&lease_body())).await;
    let before = h.call("GET", "/v1/leases", None).await.json();
    let unknown = h.call("GET", "/v1/nothing", None).await;
    h.clock.advance(std::time::Duration::from_secs(100));

    for id in ["1-1", "9-9", "abc"] {
        let path = format!("/v1/leases/{id}/renew");
        for body in [None, Some(json!({ "ttl_seconds": 60 }))] {
            let reply = h.call("POST", &path, body.as_ref()).await;
            assert_eq!(reply.status, 404, "{id}: {}", reply.body);
            assert_eq!(reply.json(), json!({ "error": "not-found" }));
            assert_eq!(
                (reply.status, reply.body),
                (unknown.status, unknown.body.clone())
            );
        }
    }
    let after = h.call("GET", "/v1/leases", None).await.json();
    assert_eq!(after, before);
    assert_eq!(after["leases"][0]["state"], "held");
    assert_eq!(after["leases"][0]["ends_at_ms"], T0 + 7_200_000);
}

/// The message of the 400 for a `ttl_seconds` that is a number.
const TTL_REMOVED: &str = "ttl_seconds was removed: a lease has no time to live; it ends when released or at max_wall_seconds";

/// Asserts that an acquire naming a number in `ttl_seconds` (60, and 0) is the 400
/// `ttl-removed` with the exact message, that nothing is recorded and no lease id is used
/// (the list is empty, and the next acquire that succeeds gets the first id), and that
/// `"ttl_seconds": null`, which the previous release's client sends on every acquire, is
/// accepted as absent. It catches a ttl that is read and ignored (a client would think it
/// set one), a refusal that books a lease or burns an id, and an older client that is
/// refused for a field it never set.
#[tokio::test]
async fn acquire_with_ttl_seconds_is_a_400_ttl_removed() {
    let h = Harness::new("ttl_removed").await;
    for ttl in [60, 0] {
        let body = with(lease_body(), "ttl_seconds", json!(ttl));
        let reply = h.call("POST", "/v1/leases", Some(&body)).await;
        assert_eq!(reply.status, 400, "{}", reply.body);
        assert_eq!(
            reply.json(),
            json!({ "error": "ttl-removed", "reason": TTL_REMOVED })
        );
    }
    assert!(ids(&h.call("GET", "/v1/leases", None).await).is_empty());

    let null = with(lease_body(), "ttl_seconds", json!(null));
    let reply = h.call("POST", "/v1/leases", Some(&null)).await;
    assert_eq!(reply.status, 201, "{}", reply.body);
    assert_eq!(reply.json()["id"], "1-1");
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
}

/// Asserts that a lease file that cannot be written fails the request with a 500 that
/// does not name the path: before the start (create), and after a change
/// (release). The write is made to fail by putting a directory where the temporary file
/// goes.
#[tokio::test]
async fn unwritable_state_is_500() {
    let h = Harness::new("unwritable").await;
    h.call("POST", "/v1/leases", Some(&lease_body())).await;
    std::fs::create_dir(h.scratch.path("leases.toml.tmp")).unwrap();

    let cases = [
        ("POST", "/v1/leases", Some(lease_body())),
        ("DELETE", "/v1/leases/1-1", None),
    ];
    for (method, path, body) in cases {
        let reply = h.call(method, path, body.as_ref()).await;
        assert_eq!(reply.status, 500, "{method} {path}");
        assert_eq!(reply.json(), json!({ "error": "state-write-failed" }));
    }
}

/// Asserts what a 500 on an acquire leaves behind: the provider was never asked to
/// start, so the refused lease is only a pending record that is listed until the next
/// tick and is dropped by it, and no host is held. Once the file can be written again
/// both hosts of the pool are still free.
#[tokio::test]
async fn a_failed_acquire_never_starts_the_lease() {
    let h = Harness::new("acquire_500").await;
    let blocker = h.scratch.path("leases.toml.tmp");
    std::fs::create_dir(&blocker).unwrap();
    let refused = h.call("POST", "/v1/leases", Some(&lease_body())).await;
    assert_eq!(refused.status, 500);
    let listed = h.call("GET", "/v1/leases", None).await.json();
    assert_eq!(listed["leases"][0]["state"], "pending");

    std::fs::remove_dir(&blocker).unwrap();
    h.ticks.step().await;
    assert!(ids(&h.call("GET", "/v1/leases", None).await).is_empty());
    for _ in 0..2 {
        let made = h.call("POST", "/v1/leases", Some(&lease_body())).await;
        assert_eq!(made.status, 201, "{}", made.body);
    }
}

/// Asserts that a 400 never repeats what the client sent: a class that is not a valid
/// id, a value of the wrong type, an unknown field, a body cut short and a body that is
/// not JSON each get a reason that names the kind of fault, and none of them contains
/// the marker the request carried (the parser's own messages would).
#[tokio::test]
async fn bad_request_reasons_do_not_echo_the_request() {
    let h = Harness::new("no_echo").await;
    let marker = "SENTINEL marker";
    let bodies: Vec<Vec<u8>> = vec![
        with(lease_body(), "class", json!(marker))
            .to_string()
            .into_bytes(),
        with(lease_body(), "max_wall_seconds", json!(marker))
            .to_string()
            .into_bytes(),
        with(lease_body(), marker, json!(1))
            .to_string()
            .into_bytes(),
        format!("{{\"class\": \"{marker}").into_bytes(),
        format!("{marker}!").into_bytes(),
    ];
    let mut reasons = Vec::new();
    for body in bodies {
        let token = Some(crate::support::TOKEN);
        let reply = request(h.addr(), "POST", "/v1/leases", token, Some(&body)).await;
        assert_eq!(reply.status, 400, "{}", reply.body);
        assert!(!reply.body.contains("SENTINEL"), "{}", reply.body);
        reasons.push(reply.json()["reason"].as_str().unwrap().to_string());
    }
    assert_eq!(reasons[0], "class is not a valid class id");
    assert!(reasons[1].starts_with("the body is JSON, but not what this route takes (line "));
    assert!(reasons[2].starts_with("the body is JSON, but not what this route takes (line "));
    assert!(reasons[3].starts_with("the body is not valid JSON (line "));
    assert!(reasons[4].starts_with("the body is not valid JSON (line "));
    let mismatch = with(lease_body(), "resources", json!({ "gpus": 1 }));
    let reply = h.call("POST", "/v1/leases", Some(&mismatch)).await;
    assert_eq!(
        reply.json()["reason"],
        "gpu_class must be named exactly when gpus is above zero"
    );
}

/// Asserts that a `disk-gib`, `exec` or `isa_level` capability that is not a requirement
/// is a 400 that names the capability and never repeats the value the client sent.
#[tokio::test]
async fn a_bad_disk_or_exec_is_a_400_that_does_not_echo_it() {
    let h = Harness::new("bad_disk_exec").await;
    let marker = "SENTINEL marker";
    for (key, said) in [
        (
            "disk-gib",
            "disk-gib must be a whole number of GiB below 2^34",
        ),
        ("exec", "exec must be container or sandbox-exec"),
        (
            "isa_level",
            "isa_level must be x86-64-v1 to x86-64-v4 or armv8.0-a to armv8.4-a",
        ),
    ] {
        let mut caps = json!({});
        caps[key] = json!(marker);
        let body = with(lease_body(), "capabilities", caps);
        let reply = h.call("POST", "/v1/leases", Some(&body)).await;
        assert_eq!(reply.status, 400, "{}", reply.body);
        assert_eq!(reply.json()["reason"], said);
        assert!(!reply.body.contains("SENTINEL"), "{}", reply.body);
    }
}
