//! Agents registering hosts: the routes, who may call them, what a leasing client sees,
//! and what hostile input gets.

use std::time::Duration;

use js_lease_server::BODY_LIMIT;
use serde_json::{Value, json};

use crate::agents_support::{
    JOIN, JOIN_B, Session, beat, entry, hello_body, hello_with, hosts, join, listed, post,
    post_raw, session, start_agents,
};
use crate::support::{Harness, LogCapture, TOKEN, guard, guard_tasks, ids, lease_body, request};

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// The reply of an empty heartbeat: no commands, nothing acknowledged.
fn empty_reply() -> Value {
    json!({ "commands": [], "result_acks": [], "taint_ack": false })
}

/// Asserts the whole path of a registered host through the lease API: before any host
/// has registered a request has no capacity (not no-fit, since the pool's shape fits it);
/// after hello and heartbeat the host takes a lease through `POST /v1/leases`, the
/// listing shows the lease on it, a second request has no capacity, and renew, release
/// and a new lease on the freed host work as on a static host. It catches a registry that
/// is not wired into the lease table, and a layer that loses the host after a release.
#[tokio::test]
async fn registered_host_is_leasable_through_post_leases() {
    let h = start_agents("agents_leasable", false).await;
    let none = h.call("POST", "/v1/leases", Some(&lease_body())).await;
    assert_eq!(none.status, 409, "{}", none.body);
    assert_eq!(none.json(), json!({ "error": "no-capacity" }));

    let me = join(&h, "burst-1").await;
    assert_eq!(me.epoch, 1);
    assert_eq!(me.secret.len(), 64);
    let heard = beat(&h, &me).await;
    assert_eq!(heard.status, 200, "{}", heard.body);
    assert_eq!(heard.json(), empty_reply());

    let granted = h.call("POST", "/v1/leases", Some(&lease_body())).await;
    assert_eq!(granted.status, 201, "{}", granted.body);
    assert_eq!(granted.json()["host"], "burst-1");
    let id = granted.json()["id"].as_str().unwrap().to_string();
    assert_eq!(entry(&hosts(&h).await, "burst-1")["lease"], id);

    let full = h.call("POST", "/v1/leases", Some(&lease_body())).await;
    assert_eq!(full.json(), json!({ "error": "no-capacity" }));
    let renewed = h
        .call("POST", &format!("/v1/leases/{id}/renew"), None)
        .await;
    assert_eq!(renewed.status, 200, "{}", renewed.body);
    let released = h.call("DELETE", &format!("/v1/leases/{id}"), None).await;
    assert_eq!(released.json(), json!({ "state": "freed" }));
    assert!(entry(&hosts(&h).await, "burst-1").get("lease").is_none());

    let again = h.call("POST", "/v1/leases", Some(&lease_body())).await;
    assert_eq!(again.status, 201, "{}", again.body);
    assert_eq!(again.json()["host"], "burst-1");
}

/// Asserts the layer's order: the two static hosts are taken first, then the registered
/// one, then there is no capacity; a freed static host is taken again before the
/// registered one. It catches a layer that prefers registered hosts or that never falls
/// through to them.
#[tokio::test]
async fn static_preferred_then_registered_when_static_full() {
    let h = start_agents("agents_layered", true).await;
    join(&h, "burst-1").await;
    let mut taken = Vec::new();
    for _ in 0..3 {
        let granted = h.call("POST", "/v1/leases", Some(&lease_body())).await;
        assert_eq!(granted.status, 201, "{}", granted.body);
        taken.push(granted.json()["host"].as_str().unwrap().to_string());
    }
    assert_eq!(taken, ["gpu-01", "gpu-02", "burst-1"]);
    let full = h.call("POST", "/v1/leases", Some(&lease_body())).await;
    assert_eq!(full.json(), json!({ "error": "no-capacity" }));

    h.call("DELETE", "/v1/leases/1-1", None).await;
    let next = h.call("POST", "/v1/leases", Some(&lease_body())).await;
    assert_eq!(next.json()["host"], "gpu-01");
}

/// Asserts that a stop on a registered host's lease is felt by the whole layer: the
/// lease is gone from the registered host and the static hosts stay as they were, and a
/// lease on a static host is not touched by the registry. It catches a layered stop that
/// reaches only the first child.
#[tokio::test]
async fn layered_stop_reaches_owning_child() {
    let h = start_agents("agents_layered_stop", true).await;
    join(&h, "burst-1").await;
    for _ in 0..3 {
        h.call("POST", "/v1/leases", Some(&lease_body())).await;
    }
    let released = h.call("DELETE", "/v1/leases/1-3", None).await;
    assert_eq!(released.json(), json!({ "state": "freed" }));
    let listing = hosts(&h).await;
    assert!(entry(&listing, "burst-1").get("lease").is_none());
    assert_eq!(entry(&listing, "gpu-01")["lease"], "1-1");
    assert_eq!(entry(&listing, "gpu-02")["lease"], "1-2");
    let renewed = h.call("POST", "/v1/leases/1-3/renew", None).await;
    assert_eq!(renewed.status, 404, "{}", renewed.body);
}

/// Asserts that an id the static pool lists cannot be registered: a hello for it is
/// refused 403 `host-listed`, no registered host appears, and the static host stays
/// leasable. It catches two hosts of one name in the table.
#[tokio::test]
async fn host_id_clash_refused() {
    let h = start_agents("agents_clash", true).await;
    let clash = hello_with(&h, JOIN, "gpu-01").await;
    assert_eq!(clash.status, 403, "{}", clash.body);
    assert_eq!(clash.json(), json!({ "error": "host-listed" }));
    let listing = hosts(&h).await;
    assert_eq!(listed(&listing), ["gpu-01", "gpu-02"]);
    assert_eq!(entry(&listing, "gpu-01")["source"], "static");
    let granted = h.call("POST", "/v1/leases", Some(&lease_body())).await;
    assert_eq!(granted.json()["host"], "gpu-01");
}

/// Asserts that the join token opens no lease route and no listing: every route of the
/// client API answers 401 with an empty body to it, whatever the method, and nothing is
/// leased. It catches the two secrets being one.
#[tokio::test]
async fn join_token_does_not_open_lease_routes() {
    let h = start_agents("agents_join_on_lease_routes", true).await;
    let routes = [
        ("GET", "/v1/leases"),
        ("POST", "/v1/leases"),
        ("POST", "/v1/leases/1-1/renew"),
        ("DELETE", "/v1/leases/1-1"),
        ("GET", "/v1/hosts"),
        ("GET", "/v1/unknown"),
    ];
    for (method, path) in routes {
        let body = lease_body().to_string().into_bytes();
        let reply = request(h.addr(), method, path, Some(JOIN), Some(&body)).await;
        assert_eq!(reply.status, 401, "{method} {path}");
        assert_eq!(reply.body, "", "{method} {path}");
    }
    assert!(ids(&h.call("GET", "/v1/leases", None).await).is_empty());
}

/// Asserts that the client token opens no agent route: both routes answer it 401 with an
/// empty body, and a hello made with it admits no host. It catches a client that can
/// register hosts, and a join token that is checked against the wrong secret.
#[tokio::test]
async fn client_token_does_not_open_agent_routes() {
    let h = start_agents("agents_client_on_agent_routes", true).await;
    let hello = post(&h, "/v1/agent/hello", Some(TOKEN), &hello_body("burst-1")).await;
    assert_eq!((hello.status, hello.body.as_str()), (401, ""));
    let beat = json!({ "host_id": "burst-1", "epoch": 1, "session": "x", "phase": "serving" });
    let heard = post(&h, "/v1/agent/heartbeat", Some(TOKEN), &beat).await;
    assert_eq!((heard.status, heard.body.as_str()), (401, ""));
    assert_eq!(listed(&hosts(&h).await), ["gpu-01", "gpu-02"]);
}

/// Asserts that a request to an agent route without a token, with an empty or wrong one,
/// with a token of another length, under another scheme, or in another header is 401 with
/// an empty body, for both routes; and that the right token passes the check.
#[tokio::test]
async fn no_token_401_empty_body() {
    let h = start_agents("agents_no_token", false).await;
    let wrong = JOIN.replace('A', "Z");
    for path in ["/v1/agent/hello", "/v1/agent/heartbeat"] {
        for token in [None, Some(""), Some("nope"), Some(wrong.as_str())] {
            let reply = request(h.addr(), "POST", path, token, Some(b"{}")).await;
            assert_eq!(
                (reply.status, reply.body.as_str()),
                (401, ""),
                "{path} {token:?}"
            );
        }
        let basic = format!("Basic {JOIN}");
        let stream = tokio::net::TcpStream::connect(h.addr()).await.unwrap();
        let reply =
            crate::support::exchange_authorized(stream, "POST", path, Some(&basic), Some(b"{}"))
                .await;
        assert_eq!(
            (reply.status, reply.body.as_str()),
            (401, ""),
            "{path} basic"
        );
        let passed = request(h.addr(), "POST", path, Some(JOIN), Some(b"{}")).await;
        assert_eq!(passed.status, 400, "{path}: {}", passed.body);
    }
}

/// Asserts that a method the agent routes do not take is a 405 for a caller with a join
/// token and a 401 for one without, and that an unknown path under the agent prefix is
/// the lease API's 404 for the client token.
#[tokio::test]
async fn agent_routes_take_post_only() {
    let h = start_agents("agents_methods", false).await;
    for path in ["/v1/agent/hello", "/v1/agent/heartbeat"] {
        let refused = request(h.addr(), "GET", path, Some(JOIN), None).await;
        assert_eq!(refused.status, 405, "{path}");
        let unauthenticated = request(h.addr(), "GET", path, None, None).await;
        assert_eq!(unauthenticated.status, 401, "{path}");
    }
    let unknown = h.call("GET", "/v1/agent/elsewhere", None).await;
    assert_eq!(unknown.status, 404);
}

/// Asserts that a server started with no agent flag has no agent route (the client
/// token gets the 404 of any unknown route, a join-shaped token the 401), lists its two
/// static hosts as static, keeps no agent file beside the lease file, and counts no
/// agent error. It catches the agent side leaking into the GPU service's path.
#[tokio::test]
async fn static_pool_alone_unchanged() {
    let h = Harness::new("agents_off").await;
    let hello = request(
        h.addr(),
        "POST",
        "/v1/agent/hello",
        Some(TOKEN),
        Some(hello_body("burst-1").to_string().as_bytes()),
    )
    .await;
    assert_eq!(hello.status, 404, "{}", hello.body);
    let joined = request(h.addr(), "POST", "/v1/agent/hello", Some(JOIN), Some(b"{}")).await;
    assert_eq!(joined.status, 401);

    let listing = hosts(&h).await;
    assert_eq!(listed(&listing), ["gpu-01", "gpu-02"]);
    assert_eq!(entry(&listing, "gpu-01")["source"], "static");
    assert_eq!(entry(&listing, "gpu-01")["shape"]["gpus"], 8);
    assert_eq!(entry(&listing, "gpu-01")["capabilities"]["driver"], "550");
    assert!(entry(&listing, "gpu-01").get("state").is_none());

    let granted = h.call("POST", "/v1/leases", Some(&lease_body())).await;
    assert_eq!(
        entry(&hosts(&h).await, "gpu-01")["lease"],
        granted.json()["id"]
    );
    h.ticks.step().await;
    let mut agent_file = h.scratch.state().into_os_string();
    agent_file.push(".agents");
    assert!(!std::path::Path::new(&agent_file).exists());
    assert_eq!(h.running.agent_errors(), 0);
}

/// Asserts that bodies that are not the message get fixed codes and nothing of what the
/// peer sent: JSON that does not parse, of the wrong type, a field of the wrong type
/// (whose serde message would quote the value), an empty body, a body cut short by the
/// peer closing, and one over the cap (413 `body-too-large`) on both routes; no host
/// appears from any of them. It catches an answer that repeats a hostile string, and a
/// decode failure that is a 500 or a hang.
#[tokio::test]
async fn malformed_and_oversized_bodies_get_fixed_codes() {
    let h = start_agents("agents_malformed", false).await;
    let marker = "SECRET-MARKER-9f3c";
    let routes = [
        ("/v1/agent/hello", "invalid-hello"),
        ("/v1/agent/heartbeat", "invalid-heartbeat"),
    ];
    for (path, code) in routes {
        let bodies = [
            "{nope".to_string(),
            "[]".to_string(),
            String::new(),
            format!("{{\"host_id\":5,\"phase\":\"{marker}\"}}"),
            format!("{{\"host_id\":\"{marker}\",\"epoch\":\"{marker}\",\"phase\":7}}"),
        ];
        for body in bodies {
            let reply = request(h.addr(), "POST", path, Some(JOIN), Some(body.as_bytes())).await;
            assert_eq!(reply.status, 400, "{path} {body}: {}", reply.body);
            assert_eq!(reply.json(), json!({ "error": code }), "{path} {body}");
            assert!(!reply.body.contains(marker));
        }
        let cut = post_raw(&h, path, JOIN, b"{\"host_id\"", Some(500)).await;
        assert_eq!(cut.status, 400, "{path}: {}", cut.body);
        assert_eq!(
            serde_json::from_str::<Value>(&cut.body).unwrap(),
            json!({ "error": code })
        );

        let big = vec![b' '; BODY_LIMIT + 1];
        let over = request(h.addr(), "POST", path, Some(JOIN), Some(&big)).await;
        assert_eq!(over.status, 413, "{path}");
        assert_eq!(over.json(), json!({ "error": "body-too-large" }));
    }
    assert!(listed(&hosts(&h).await).is_empty());
}

/// Asserts that every message is validated before the registry sees it: a hello with a
/// reserved label, a host id outside the alphabet, a shape above the wire's caps or an
/// unknown protocol range, and a heartbeat with a malformed host id or session, are each
/// answered 400 with the wire's code and none of the offending text, and admit nothing.
/// An unsupported protocol also says which majors the server serves. It catches a hello
/// that reaches the registry unchecked.
#[tokio::test]
async fn every_decoded_message_is_validated_before_use() {
    let h = start_agents("agents_validated", false).await;
    let marker = "hostile-marker";
    let mut bad_label = hello_body("burst-1");
    bad_label["labels"] = json!({ "jobscheduler.dev/owner": marker });
    let mut bad_id = hello_body(marker);
    bad_id["host_id"] = json!(format!("UPPER {marker}"));
    let mut huge = hello_body("burst-1");
    huge["shape"]["cpu_millis"] = json!(u64::MAX);
    let mut protocol = hello_body("burst-1");
    protocol["protocol"] = json!({ "min": 2, "max": 3 });
    let cases = [
        (bad_label, 400, "reserved-label"),
        (bad_id, 400, "invalid-hello"),
        (huge, 400, "invalid-hello"),
        (protocol, 400, "unsupported-protocol"),
    ];
    for (body, status, code) in cases {
        let reply = post(&h, "/v1/agent/hello", Some(JOIN), &body).await;
        assert_eq!(reply.status, status, "{code}: {}", reply.body);
        assert_eq!(reply.json()["error"], code);
        assert!(!reply.body.contains(marker), "{}", reply.body);
        if code == "unsupported-protocol" {
            assert_eq!(reply.json()["supported"], json!({ "min": 1, "max": 1 }));
        } else {
            assert!(reply.json().get("supported").is_none());
        }
    }
    let beats = [
        json!({ "host_id": format!("UPPER {marker}"), "epoch": 1, "phase": "serving" }),
        json!({ "host_id": "burst-1", "epoch": 1, "session": format!("bad {marker}"), "phase": "serving" }),
    ];
    for body in beats {
        let reply = post(&h, "/v1/agent/heartbeat", Some(JOIN), &body).await;
        assert_eq!(reply.status, 400, "{}", reply.body);
        assert_eq!(reply.json(), json!({ "error": "invalid-heartbeat" }));
    }
    assert!(listed(&hosts(&h).await).is_empty());
}

/// Asserts each registry refusal on the wire with the status the API documents and
/// nothing but the code in the body: unknown pool, shape not accepted and a full pool
/// (403), a hello while the host is live (409), and on the heartbeat an unknown host and
/// a stale epoch (409), a missing and a wrong session (403). It catches a status that
/// tells an agent to say hello when it should wait, and a code that is not the wire's.
#[tokio::test]
async fn refusals_have_their_status_and_code() {
    let h = start_agents("agents_refusals", false).await;
    let a = join(&h, "burst-1").await;
    let mut elsewhere = hello_body("burst-2");
    elsewhere["pool"] = json!("other");
    let mut small = hello_body("burst-2");
    small["shape"]["cpu_millis"] = json!(1);
    let hellos = [
        (hello_body("burst-1"), 409, "host-live"),
        (elsewhere, 403, "unknown-pool"),
        (small, 403, "shape-not-accepted"),
    ];
    for (body, status, code) in hellos {
        let reply = post(&h, "/v1/agent/hello", Some(JOIN), &body).await;
        assert_eq!(reply.status, status, "{code}: {}", reply.body);
        assert_eq!(reply.json(), json!({ "error": code }));
    }
    join(&h, "burst-2").await;
    let full = hello_with(&h, JOIN, "burst-3").await;
    assert_eq!(
        (full.status, full.json()),
        (403, json!({ "error": "pool-full" }))
    );

    let ghost = Session {
        host: "burst-9".into(),
        ..a.clone()
    };
    let stale = Session {
        epoch: a.epoch + 1,
        ..a.clone()
    };
    let wrong = Session {
        secret: "0".repeat(64),
        ..a.clone()
    };
    let missing = Session {
        secret: String::new(),
        ..a.clone()
    };
    let beats = [
        (ghost, 409, "unknown-host"),
        (stale, 409, "stale-epoch"),
        (wrong, 403, "session-mismatch"),
        (missing, 403, "session-mismatch"),
    ];
    for (session, status, code) in beats {
        let reply = beat(&h, &session).await;
        assert_eq!(reply.status, status, "{code}: {}", reply.body);
        assert_eq!(reply.json(), json!({ "error": code }));
    }
    assert_eq!(beat(&h, &a).await.status, 200);
}

/// Asserts the listing of registered hosts: id, source, state, phase, lease, shape,
/// capabilities, labels and agent version, with the static hosts beside them in id order;
/// and that a host silent past the grace period reads `lost` with `lost: true`, and live
/// again after a heartbeat. It catches a listing that hides a lost host or shows the
/// host's own claim instead of the shape that is booked.
#[tokio::test]
async fn hosts_lists_registered_hosts_with_their_state() {
    let h = start_agents("agents_hosts", true).await;
    let a = join(&h, "burst-1").await;
    let listing = hosts(&h).await;
    assert_eq!(listed(&listing), ["burst-1", "gpu-01", "gpu-02"]);
    let host = entry(&listing, "burst-1");
    assert_eq!(host["source"], "registered");
    assert_eq!(host["state"], "live");
    assert_eq!(host["lost"], false);
    assert_eq!(host["phase"], "serving");
    assert_eq!(host["agent_version"], "0.1.0");
    assert_eq!(host["labels"], json!({ "rack": "r1" }));
    assert_eq!(host["capabilities"], json!({ "isa": "x86-64-v3" }));
    assert_eq!(
        host["shape"],
        json!({ "cpu_millis": 8000, "memory_bytes": 17179869184u64, "gpus": 1, "gpu_class": "nvidia-a100" })
    );
    assert!(host.get("lease").is_none());

    h.clock.advance(secs(16));
    let lost = entry(&hosts(&h).await, "burst-1");
    assert_eq!(
        (lost["state"].clone(), lost["lost"].clone()),
        (json!("lost"), json!(true))
    );
    assert_eq!(beat(&h, &a).await.status, 200);
    assert_eq!(entry(&hosts(&h).await, "burst-1")["state"], "live");
}

/// Asserts that a lease on a host that went silent stays held (the table sees `Unknown`,
/// not `Absent`) through ticks, a request cannot book the lost host, and the lease then
/// ends by its ttl at a tick, after which the host, once it says hello again, takes a
/// new lease. It catches a lost host whose lease is freed early, or never.
#[tokio::test]
async fn lost_registered_host_lease_expires_by_ttl() {
    let h = start_agents("agents_lost", false).await;
    join(&h, "burst-1").await;
    let granted = h.call("POST", "/v1/leases", Some(&lease_body())).await;
    assert_eq!(
        granted.json()["expires_at_ms"],
        crate::support::T0 + 600_000
    );

    h.clock.advance(secs(30));
    h.ticks.step().await;
    assert_eq!(ids(&h.call("GET", "/v1/leases", None).await), ["1-1"]);
    assert_eq!(entry(&hosts(&h).await, "burst-1")["state"], "lost");
    let renewed = h.call("POST", "/v1/leases/1-1/renew", None).await;
    assert_eq!(renewed.status, 200, "{}", renewed.body);

    h.clock.advance(secs(601));
    h.ticks.step().await;
    assert!(ids(&h.call("GET", "/v1/leases", None).await).is_empty());
    assert!(entry(&hosts(&h).await, "burst-1").get("lease").is_none());

    let back = join(&h, "burst-1").await;
    assert_eq!(back.epoch, 2);
    let again = h.call("POST", "/v1/leases", Some(&lease_body())).await;
    assert_eq!(again.status, 201, "{}", again.body);
}

/// Asserts that heartbeats from several agents and lease calls and listings that take
/// the table's lock run side by side on a multi-threaded runtime and all complete, with
/// the answers they should have. The table's lock is taken before the registry's and the
/// agent handlers take only the registry's; a handler that took them the other way round
/// would deadlock here. It asserts completion and answers, never time.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn heartbeats_hammered_during_lease_calls_complete() {
    let h = start_agents("agents_hammer", false).await;
    let sessions = [join(&h, "burst-1").await, join(&h, "burst-2").await];
    let addr = h.addr();
    let mut tasks = Vec::new();
    for (name, session) in ["beat-1", "beat-2", "beat-1-again", "beat-2-again"]
        .into_iter()
        .zip(sessions.iter().chain(sessions.iter()))
    {
        let body = session.beat().to_string().into_bytes();
        tasks.push((
            name,
            tokio::spawn(async move {
                for _ in 0..40 {
                    let path = "/v1/agent/heartbeat";
                    let reply = request(addr, "POST", path, Some(JOIN), Some(&body)).await;
                    assert_eq!(reply.status, 200, "{}", reply.body);
                }
            }),
        ));
    }
    tasks.push((
        "lease-calls",
        tokio::spawn(async move {
            for _ in 0..30 {
                let body = lease_body().to_string().into_bytes();
                let made = request(addr, "POST", "/v1/leases", Some(TOKEN), Some(&body)).await;
                assert_eq!(made.status, 201, "{}", made.body);
                let id = made.json()["id"].as_str().unwrap().to_string();
                let renew = format!("/v1/leases/{id}/renew");
                let renewed = request(addr, "POST", &renew, Some(TOKEN), None).await;
                assert_eq!(renewed.status, 200);
                let path = format!("/v1/leases/{id}");
                let freed = request(addr, "DELETE", &path, Some(TOKEN), None).await;
                assert_eq!(freed.status, 200);
            }
        }),
    ));
    tasks.push((
        "host-listings",
        tokio::spawn(async move {
            for _ in 0..30 {
                let listing = request(addr, "GET", "/v1/hosts", Some(TOKEN), None).await;
                assert_eq!(listing.status, 200);
            }
        }),
    ));
    guard_tasks(tasks).await;
}

/// Asserts the hello rate limit at the transport, per credential, on the injected clock:
/// a token that spends its burst gets 429 `rate-limited` with a `Retry-After` in whole
/// seconds (refusals count, so a loop of bad hellos is held too), the same token's
/// heartbeats and another token's hellos are not held, and moving the clock by the
/// interval lets one hello through again. It catches a limiter that never limits, one
/// shared by all tokens, one that also holds heartbeats (hosts would be lost), and one
/// on the system clock.
#[tokio::test]
async fn hello_rate_limit_holds_a_credential_not_the_others() {
    let scratch = crate::support::Scratch::new("agents_rate");
    let mut args = crate::agents_support::agent_args(&scratch, false);
    args.extend(["--agent-hello-burst", "2", "--agent-hello-interval", "10s"].map(Into::into));
    let h = Harness::with(scratch, args, None).await;

    let a = join(&h, "burst-1").await;
    let live = hello_with(&h, JOIN, "burst-1").await;
    assert_eq!(live.status, 409);
    let held = post_raw(
        &h,
        "/v1/agent/hello",
        JOIN,
        hello_body("burst-1").to_string().as_bytes(),
        None,
    )
    .await;
    assert_eq!(held.status, 429, "{}", held.body);
    assert_eq!(
        serde_json::from_str::<Value>(&held.body).unwrap(),
        json!({ "error": "rate-limited" })
    );
    assert!(held.head.contains("retry-after: 10"), "{}", held.head);

    assert_eq!(beat(&h, &a).await.status, 200);
    let other = hello_with(&h, JOIN_B, "burst-2").await;
    assert_eq!(other.status, 200, "{}", other.body);

    h.clock.advance(secs(10));
    let once = hello_with(&h, JOIN, "burst-1").await;
    assert_eq!(once.status, 409);
    let again = hello_with(&h, JOIN, "burst-1").await;
    assert_eq!(again.status, 429);
    h.clock.advance(secs(3));
    let still = post_raw(&h, "/v1/agent/hello", JOIN, b"{}", None).await;
    assert!(still.head.contains("retry-after: 7"), "{}", still.head);
    assert_eq!(session(&other, "burst-2").epoch, 1);
}

/// Asserts that a token holder who keeps saying hello past its limit gets a 429 each
/// time but is logged once per interval, not once per request: many refusals in one
/// interval leave one line, and the first refusal of the next interval leaves another.
/// It catches a warning per refusal, which lets a holder of a join token fill the log at
/// the speed of its requests, and a limiter that never says so.
#[tokio::test]
async fn refusals_for_the_hello_limit_are_logged_once_per_interval() {
    let logs = LogCapture::start();
    let scratch = crate::support::Scratch::new("agents_rate_log");
    let mut args = crate::agents_support::agent_args(&scratch, false);
    args.extend(["--agent-hello-burst", "1", "--agent-hello-interval", "10s"].map(Into::into));
    let h = Harness::with(scratch, args, None).await;
    let held = "held to the rate limit";
    assert_eq!(hello_with(&h, JOIN, "burst-1").await.status, 200);
    for _ in 0..5 {
        assert_eq!(hello_with(&h, JOIN, "burst-1").await.status, 429);
    }
    assert_eq!(logs.text().matches(held).count(), 1, "{}", logs.text());
    assert_eq!(hello_with(&h, JOIN_B, "burst-2").await.status, 200);
    for _ in 0..3 {
        assert_eq!(hello_with(&h, JOIN_B, "burst-2").await.status, 429);
    }
    assert_eq!(logs.text().matches(held).count(), 2, "{}", logs.text());

    h.clock.advance(secs(10));
    assert_eq!(hello_with(&h, JOIN, "burst-1").await.status, 409);
    for _ in 0..3 {
        assert_eq!(hello_with(&h, JOIN, "burst-1").await.status, 429);
    }
    assert_eq!(logs.text().matches(held).count(), 3, "{}", logs.text());
}

/// Asserts that a static source supplied by the caller (a provider of its own, as the
/// library allows) is layered under the registered hosts exactly as the pool file is:
/// tried first, its ids refused to registering hosts. It catches a layer that works only
/// for the source the flags build.
#[tokio::test]
async fn a_supplied_static_source_is_layered_like_the_pool_file() {
    let scratch = crate::support::Scratch::new("agents_supplied");
    let args = crate::agents_support::agent_args(&scratch, false);
    let supplied = Box::new(crate::support::SharedPool::new());
    let h = Harness::with(scratch, args, Some(supplied)).await;
    join(&h, "burst-1").await;
    let mut taken = Vec::new();
    for _ in 0..3 {
        let granted = h.call("POST", "/v1/leases", Some(&lease_body())).await;
        assert_eq!(granted.status, 201, "{}", granted.body);
        taken.push(granted.json()["host"].as_str().unwrap().to_string());
    }
    assert_eq!(taken, ["gpu-01", "gpu-02", "burst-1"]);
    let clash = hello_with(&h, JOIN, "gpu-02").await;
    assert_eq!(clash.json(), json!({ "error": "host-listed" }));
}
