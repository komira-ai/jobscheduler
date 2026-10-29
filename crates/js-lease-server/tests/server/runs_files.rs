//! Run files through the run API: they reach the agent's start only on a host that
//! reports `run-files`, a bad file is a 400 before anything is queued, and the contents
//! are never echoed back.

use serde_json::{Value, json};

use crate::agents_support::{JOIN, Session, hello_body, post, session};
use crate::runs_support::{beat, lease, queue, run_body, runs, words};
use crate::support::Harness;

const SECRET: &str = "https://store.example/put?sig=SECRET-VALUE-123";

/// A host that joins, reporting `run-files` when `with_files`.
async fn join_host(h: &Harness, with_files: bool) -> Session {
    let mut body = hello_body("burst-1");
    if with_files {
        body["capabilities"]["run-files"] = json!("true");
    }
    let reply = post(h, "/v1/agent/hello", Some(JOIN), &body).await;
    session(&reply, "burst-1")
}

fn body_with(files: Value) -> Value {
    let mut body = run_body(1000);
    body["files"] = files;
    body
}

fn secret_file() -> Value {
    json!([{ "name": "put-url", "contents": SECRET }])
}

/// Asserts the path of a file end to end: the POST is accepted, the next heartbeat reply
/// carries the start with the file's name and contents unchanged, and neither the POST
/// answer nor the run listing holds the contents. It catches a handler that drops `files`
/// before the job is built, and one that echoes the secret to the client or lists it.
#[tokio::test]
async fn files_reach_the_start_and_are_not_echoed() {
    let h = crate::agents_support::start_agents("run_files_flow", false).await;
    let me = join_host(&h, true).await;
    let lease = lease(&h).await;
    let queued = queue(&h, &lease, &body_with(secret_file())).await;
    assert_eq!(queued.status, 202, "{}", queued.body);
    assert!(!queued.body.contains("SECRET-VALUE"), "{}", queued.body);
    let reply = beat(&h, &me, &[], &[]).await;
    assert_eq!(words(&reply), ["start run-1-1"]);
    assert_eq!(reply["commands"][0]["files"], secret_file());
    let listing = runs(&h, &lease).await;
    assert!(
        !format!("{listing:?}").contains("SECRET-VALUE"),
        "{listing:?}"
    );
}

/// Asserts that a run without files sends no `files` key, so the start is the bytes an
/// older agent has always read. It catches a field that is always serialised.
#[tokio::test]
async fn a_start_without_files_has_no_files_key() {
    let h = crate::agents_support::start_agents("run_files_none", false).await;
    let me = join_host(&h, false).await;
    let lease = lease(&h).await;
    let queued = queue(&h, &lease, &run_body(1000)).await;
    assert_eq!(queued.status, 202, "{}", queued.body);
    let reply = beat(&h, &me, &[], &[]).await;
    assert!(reply["commands"][0].get("files").is_none(), "{reply}");
}

/// Asserts that files for a host that did not report `run-files` are 409
/// `files-unsupported`, nothing is queued, and the start never goes out. It catches the
/// fail-open case: an older agent ignores the field and would run the task without its
/// URL.
#[tokio::test]
async fn files_for_an_agent_without_the_capability_are_refused() {
    let h = crate::agents_support::start_agents("run_files_old", false).await;
    let me = join_host(&h, false).await;
    let lease = lease(&h).await;
    let refused = queue(&h, &lease, &body_with(secret_file())).await;
    assert_eq!(
        (refused.status, refused.json()),
        (409, json!({ "error": "files-unsupported" }))
    );
    assert!(runs(&h, &lease).await.is_empty());
    let reply = beat(&h, &me, &[], &[]).await;
    assert!(words(&reply).is_empty());
}

/// Asserts that each kind of bad file is a 400 `invalid-file`, before the lease and the
/// host are looked at (the host here lacks the capability, which would be a 409), and that
/// the answer repeats neither the name nor the contents. It catches a file check that runs
/// after the host check, a 422 for what is the body's fault, and an error that echoes a
/// secret.
#[tokio::test]
async fn a_bad_file_is_a_400() {
    let h = crate::agents_support::start_agents("run_files_bad", false).await;
    join_host(&h, false).await;
    let lease = lease(&h).await;
    let many: Vec<Value> = (0..9)
        .map(|i| json!({ "name": format!("f{i}"), "contents": "x" }))
        .collect();
    let big = "x".repeat(16 * 1024 + 1);
    let cases = [
        json!([{ "name": "../x", "contents": SECRET }]),
        json!([{ "name": "a", "contents": SECRET }, { "name": "a", "contents": "y" }]),
        json!([{ "name": "a", "contents": format!("{SECRET}\u{0}") }]),
        json!([{ "name": "secret-name", "contents": big }]),
        json!(many),
    ];
    for files in cases {
        let reply = queue(&h, &lease, &body_with(files.clone())).await;
        assert_eq!(
            (reply.status, reply.json()),
            (400, json!({ "error": "invalid-file" })),
            "{files}"
        );
        assert!(!reply.body.contains("SECRET-VALUE"), "{}", reply.body);
        assert!(!reply.body.contains("secret-name"), "{}", reply.body);
    }
    assert!(runs(&h, &lease).await.is_empty());
}
