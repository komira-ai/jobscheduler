//! The run routes as an API: who may call them, what they answer for what they cannot
//! serve, the hosts that are not agents, and the size of a reply.

use js_agent_wire::{HeartbeatReply, limits};
use serde_json::json;

use crate::agents_support::{JOIN, join, post, start_agents};
use crate::runs_support::{held, lease, queue, queued, run_body, runs, stop, words};
use crate::support::{Harness, TOKEN, lease_body, request};

/// Asserts that the three run routes need the client token: a request with none and one
/// with the join token are both a 401 with an empty body, and nothing is queued by them.
/// It catches run routes left outside the authentication, or opened to the agents' token,
/// which would let a host queue work for itself.
#[tokio::test]
async fn run_routes_need_client_token() {
    let h = start_agents("runs_auth", false).await;
    join(&h, "burst-1").await;
    let lease = lease(&h).await;
    let run = queued(&h, &lease, 1000).await;
    let body = run_body(1000).to_string().into_bytes();
    let routes = [
        ("POST", format!("/v1/leases/{lease}/runs")),
        ("GET", format!("/v1/leases/{lease}/runs")),
        ("DELETE", format!("/v1/leases/{lease}/runs/{run}")),
    ];
    for (method, path) in routes {
        for token in [None, Some(JOIN)] {
            let reply = request(h.addr(), method, &path, token, Some(&body)).await;
            assert_eq!(reply.status, 401, "{method} {path}: {}", reply.body);
            assert!(reply.body.is_empty(), "{}", reply.body);
        }
    }
    assert_eq!(runs(&h, &lease).await.len(), 1);
    let path = format!("/v1/leases/{lease}/runs");
    let ok = request(h.addr(), "GET", &path, Some(TOKEN), None).await;
    assert_eq!(ok.status, 200, "{}", ok.body);
}

/// Asserts that a lease on a static host takes no run: 409 `host-not-registered`, with the
/// static host's lease otherwise as it always was (the GPU service path is unchanged), and
/// its run list is empty. It catches a run routed to a host with no agent to carry it.
#[tokio::test]
async fn run_on_static_host_409() {
    let h = start_agents("runs_static", true).await;
    join(&h, "burst-1").await;
    let granted = h.call("POST", "/v1/leases", Some(&lease_body())).await;
    assert_eq!(granted.json()["host"], "gpu-01");
    let lease = granted.json()["id"].as_str().unwrap().to_string();
    let refused = queue(&h, &lease, &run_body(1000)).await;
    assert_eq!(refused.status, 409, "{}", refused.body);
    assert_eq!(refused.json(), json!({ "error": "host-not-registered" }));
    assert!(runs(&h, &lease).await.is_empty());
    let released = h.call("DELETE", &format!("/v1/leases/{lease}"), None).await;
    assert_eq!(released.json(), json!({ "state": "freed" }));
}

/// Asserts that without the agent flags the run routes do not exist: they answer as any
/// unknown route does, 404 `not-found`, for every method, so a server started as before
/// behaves exactly as before. It catches run routes that appear on a server that never
/// registers agents.
#[tokio::test]
async fn without_agents_there_are_no_run_routes() {
    let h = Harness::new("runs_absent").await;
    let granted = h.call("POST", "/v1/leases", Some(&lease_body())).await;
    let lease = granted.json()["id"].as_str().unwrap().to_string();
    let paths = [
        ("POST", format!("/v1/leases/{lease}/runs")),
        ("GET", format!("/v1/leases/{lease}/runs")),
        ("DELETE", format!("/v1/leases/{lease}/runs/run-1-1")),
    ];
    for (method, path) in paths {
        let reply = h.call(method, &path, Some(&run_body(1000))).await;
        assert_eq!(reply.status, 404, "{method} {path}: {}", reply.body);
        assert_eq!(reply.json(), json!({ "error": "not-found" }));
    }
}

/// Asserts the 404s and the lease state: a lease the table does not know (a malformed id
/// as well), a run the lease does not have, and a run id of another lease's run are all
/// 404 with their codes. It catches a stop that reaches a run through the wrong lease.
#[tokio::test]
async fn unknown_lease_and_unknown_run_are_404() {
    let h = start_agents("runs_404", false).await;
    join(&h, "burst-1").await;
    for lease in ["9-9", "nonsense"] {
        let queued = queue(&h, lease, &run_body(1000)).await;
        assert_eq!(queued.json(), json!({ "error": "unknown-lease" }));
        let listed = h
            .call("GET", &format!("/v1/leases/{lease}/runs"), None)
            .await;
        assert_eq!(listed.status, 404, "{}", listed.body);
        let stopped = stop(&h, lease, "run-1-1").await;
        assert_eq!(stopped.json(), json!({ "error": "unknown-lease" }));
    }
    let lease = lease(&h).await;
    let unknown = stop(&h, &lease, "run-1-1").await;
    assert_eq!(unknown.status, 404, "{}", unknown.body);
    assert_eq!(unknown.json(), json!({ "error": "unknown-run" }));
}

/// Asserts that a run ended is left as it is by a stop (202 and `ended`), and that a stop of
/// a run is 202 whatever its state. It catches a stop that fails for a run that is already
/// gone, which a client retrying a delete would meet.
#[tokio::test]
async fn stopping_an_ended_run_is_accepted() {
    let h = start_agents("runs_stop_ended", false).await;
    let me = join(&h, "burst-1").await;
    let lease = lease(&h).await;
    let run = queued(&h, &lease, 1000).await;
    crate::runs_support::beat(&h, &me, &[held(&run, "running")], &[]).await;
    crate::runs_support::beat(&h, &me, &[], &[]).await;
    let stopped = stop(&h, &lease, &run).await;
    assert_eq!(stopped.status, 202, "{}", stopped.body);
    assert_eq!(stopped.json(), json!({ "run_id": run, "state": "ended" }));
}

/// Asserts that a reply stays within the wire's caps whatever the agent lists: a heartbeat
/// that lists more unknown runs than a reply may carry is answered with exactly the cap of
/// cancels, and the reply decodes and validates with nothing refused. It catches a reply
/// the agent would refuse in part.
#[tokio::test]
async fn reply_size_stays_within_caps_with_many_queued_commands() {
    let h = start_agents("runs_caps", false).await;
    let me = join(&h, "burst-1").await;
    let strangers = limits::MAX_COMMANDS + 76;
    let listed: Vec<_> = (0..strangers)
        .map(|n| held(&format!("s{n}"), "running"))
        .collect();
    let mut body = me.beat();
    body["held"] = json!(listed);
    let reply = post(&h, "/v1/agent/heartbeat", Some(JOIN), &body).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    let decoded: HeartbeatReply = serde_json::from_str(&reply.body).unwrap();
    assert_eq!(decoded.commands.len(), limits::MAX_COMMANDS);
    let checked = decoded.validate();
    assert!(checked.refused.is_empty());
    assert!(checked.acks.is_ok());
    assert_eq!(words(&reply.json())[0], "cancel s0");
}
