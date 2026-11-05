//! What the cell's test binaries share: virtual times, scratch directories, and the
//! client calls that make a lease and a run on the real server.

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

/// A client request with the client token and `body` as JSON.
pub fn request(method: &str, path: &str, body: &Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {CLIENT_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
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

/// A lease of one whole host of the shape `Shape::gpu_host` books (the request the
/// server's own tests make), when the server grants it: its id.
pub fn lease(cell: &mut Cell) -> Option<String> {
    let body = json!({
        "class": "gpu-host",
        "resources": { "cpu_millis": 1000, "memory_bytes": 1024, "gpus": 1, "gpu_class": "nvidia-a100" },
        "capabilities": { "isa": "x86-64-v3" },
        "max_wall_seconds": 7200,
        "labels": { "team": "infra" },
    });
    let (status, answer) = call(cell, "POST", "/v1/leases", &body);
    let id = answer["id"].as_str().map(str::to_owned);
    id.filter(|_| status == 201)
}

/// A run on `lease` of [`SIM_RUN_IMAGE`] that ends by itself after `millis`, when the
/// server queues it: its id.
pub fn start_run(cell: &mut Cell, lease: &str, millis: u64) -> Option<String> {
    let body = json!({
        "image": SIM_RUN_IMAGE,
        "args": [millis.to_string()],
        "limits": { "cpu_millis": 2000, "memory_bytes": 1_048_576 },
    });
    let (status, answer) = call(cell, "POST", &format!("/v1/leases/{lease}/runs"), &body);
    let id = answer["run_id"].as_str().map(str::to_owned);
    id.filter(|_| status == 202)
}
