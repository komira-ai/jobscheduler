//! What the cell's test binaries share: virtual times, scratch directories, and the
//! client calls that make a job on the real server.

use std::path::Path;

use axum::body::Body;
use axum::http::Request;
use js_sim_cell::server::CLIENT_TOKEN;
use js_sim_cell::{Cell, SIM_RUN_IMAGE};
use js_testkit::dir::TestDir;
use js_types::sched::FarmTime;
use serde_json::{Value, json};

/// Virtual time `n` milliseconds after the start.
pub fn ms(n: u64) -> FarmTime {
    FarmTime::from_millis(n)
}

/// A scratch directory of its own for one server, removed when the server is dropped.
pub fn scratch(name: &str) -> TestDir {
    TestDir::under(
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("js-sim-cell"),
        name,
    )
}

/// A client request with the client token and `body` as JSON (none for `Null`).
pub fn request(method: &str, path: &str, body: &Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {CLIENT_TOKEN}"))
        .header("content-type", "application/json")
        .body(if body.is_null() {
            Body::empty()
        } else {
            Body::from(body.to_string())
        })
        .unwrap()
}

/// One call through the cell's client router, now: the status and the body as JSON
/// (`Null` when it is not JSON).
pub fn call(cell: &mut Cell, method: &str, path: &str, body: &Value) -> (u16, Value) {
    let answer = cell
        .client_call(request(method, path, body))
        .expect("the cell has a server");
    let status = answer.status().as_u16();
    (
        status,
        serde_json::from_str(answer.body()).unwrap_or(Value::Null),
    )
}

/// A job `id` of [`SIM_RUN_IMAGE`] that ends by itself after `millis`, asking for one core
/// and 1 MiB of memory, when the job service accepts it: its id.
pub fn job(cell: &mut Cell, id: &str, millis: u64) -> Option<String> {
    let body = json!({
        "spec": {
            "jobId": id,
            "image": SIM_RUN_IMAGE,
            "args": [millis.to_string()],
            "needs": { "cpuMillis": "1000", "memoryBytes": "1048576" },
            "wallLimitSeconds": "7200",
        }
    });
    let (status, _) = call(cell, "POST", "/v1/jobs", &body);
    (status == 201).then(|| id.to_owned())
}
