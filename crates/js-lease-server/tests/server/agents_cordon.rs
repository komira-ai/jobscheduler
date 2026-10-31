//! The own-host routes: a host cordons itself, lifts its own cordon and reads its own
//! state with the credential it was admitted with (`POST /v1/hosts/{id}:cordon`,
//! `POST /v1/hosts/{id}:uncordon`, `GET /v1/hosts/{id}`). A cordon is a lease that the
//! requester sets and renews, that the server caps and lets lapse, and that a restart keeps.

use std::fs;
use std::time::Duration;

use serde_json::{Value, json};

use crate::agents_support::{JOIN, Session, hello_body, post, restarted, session, start_agents};
use crate::joins::{beat_with, code, minted};
use crate::support::{Harness, Reply, T0, lease_body, request};

/// The default length of a cordon asked for with no length, and the longest one a request
/// can ask for, in seconds.
const DEFAULT_SECS: u64 = 300;
const MAX_SECS: u64 = 3600;

/// A host and what it holds after its hello.
struct Host {
    credential: String,
    session: Session,
}

/// The hello of `host` on `instance` that asks for a host credential and reports a runtime.
fn asking(host: &str, instance: &str) -> Value {
    let mut body = hello_body(host);
    body["instance"] = json!({ "provider": "lambdalabs", "instance_id": instance });
    body["accepts_host_credential"] = json!(true);
    body["capabilities"]["runtime"] = json!("container");
    body
}

/// A host admitted with a one-time token that it swapped for a host credential.
async fn enrolled(h: &Harness, host: &str, instance: &str) -> Host {
    let token = minted(h, instance).await;
    let welcome = post(h, "/v1/agent/hello", Some(&token), &asking(host, instance)).await;
    let credential = welcome.json()["host_credential"]
        .as_str()
        .unwrap()
        .to_string();
    Host {
        credential,
        session: session(&welcome, host),
    }
}

/// The path of an own-host route: `verb` is `cordon` or `uncordon`, or none for the read.
fn path(host: &str, verb: Option<&str>) -> String {
    match verb {
        Some(verb) => format!("/v1/hosts/{host}:{verb}"),
        None => format!("/v1/hosts/{host}"),
    }
}

/// A call of an own-host route with `secret` as the bearer.
async fn own(
    h: &Harness,
    method: &str,
    host: &str,
    verb: Option<&str>,
    secret: &str,
    body: Option<&Value>,
) -> Reply {
    let bytes = body.map(|body| body.to_string().into_bytes());
    request(
        h.addr(),
        method,
        &path(host, verb),
        Some(secret),
        bytes.as_deref(),
    )
    .await
}

/// `host` cordons itself for `secs` seconds.
async fn cordon(h: &Harness, host: &str, me: &Host, secs: u64) -> Reply {
    let body = json!({ "lease_secs": secs });
    own(h, "POST", host, Some("cordon"), &me.credential, Some(&body)).await
}

/// `host` reads its own state.
async fn show(h: &Harness, host: &str, me: &Host) -> Value {
    let reply = own(h, "GET", host, None, &me.credential, None).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    reply.json()
}

/// `host` lifts its own cordon.
async fn uncordon(h: &Harness, host: &str, me: &Host) -> Reply {
    own(h, "POST", host, Some("uncordon"), &me.credential, None).await
}

/// Lets `secs` seconds pass in steps of one heartbeat interval, the host heartbeating at
/// each, so that it is live throughout.
async fn live_for(h: &Harness, me: &Host, secs: u64) {
    for _ in 0..secs / 5 {
        h.clock.advance(Duration::from_secs(5));
        let beat = beat_with(h, &me.credential, &me.session).await;
        assert_eq!(beat.status, 200, "{}", beat.body);
    }
}

/// Asks for a lease; the answer is the reply.
async fn lease(h: &Harness) -> Reply {
    h.call("POST", "/v1/leases", Some(&lease_body())).await
}

fn no_capacity(reply: &Reply) -> bool {
    reply.json() == json!({ "error": "no-capacity" })
}

/// Asserts that the routes serve the host of the credential and no other: the credential
/// of one host is refused 403 `join-token-not-admitted` on another host's read, cordon and
/// uncordon, a pool join token is refused 403 `host-credential-required` on its own host's
/// (it names no host), the client token and no token are 401, and the host that was reached
/// for is neither cordoned nor kept from a lease. It catches a route that trusts the host
/// id in the path, one that lets the shared pool token act for any host, and one that opens
/// to the client token.
#[tokio::test]
async fn a_host_credential_serves_its_own_host_only() {
    let h = start_agents("cordon_foreign", false).await;
    let one = enrolled(&h, "burst-1", "i-1").await;
    let two = enrolled(&h, "burst-2", "i-2").await;
    for (method, verb) in [
        ("GET", None),
        ("POST", Some("cordon")),
        ("POST", Some("uncordon")),
    ] {
        let foreign = own(&h, method, "burst-2", verb, &one.credential, None).await;
        assert_eq!(foreign.status, 403, "{method} {verb:?}: {}", foreign.body);
        assert_eq!(code(&foreign), "join-token-not-admitted");
        let pooled = own(&h, method, "burst-1", verb, JOIN, None).await;
        assert_eq!(pooled.status, 403, "{method} {verb:?}: {}", pooled.body);
        assert_eq!(code(&pooled), "host-credential-required");
        let client = own(&h, method, "burst-1", verb, crate::support::TOKEN, None).await;
        assert_eq!(client.status, 401, "{method} {verb:?}: {}", client.body);
        let bare = request(h.addr(), method, &path("burst-1", verb), None, None).await;
        assert_eq!(bare.status, 401, "{method} {verb:?}: {}", bare.body);
    }
    let unnamed = own(&h, "GET", "UPPER", None, &one.credential, None).await;
    assert_eq!(unnamed.status, 403, "{}", unnamed.body);

    assert!(show(&h, "burst-2", &two).await.get("cordon").is_none());
    let hosts: Vec<_> = [lease(&h).await, lease(&h).await]
        .iter()
        .map(|reply| reply.json()["host"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(hosts.len(), 2);
    assert!(hosts.contains(&"burst-1".to_string()) && hosts.contains(&"burst-2".to_string()));
}

/// Asserts what a cordon does to placement: a cordoned host is offered no new lease (the
/// request is `no-capacity`), the answer says the host was not cordoned before, and after
/// an uncordon the same request is granted on that host. It catches a cordon that is
/// recorded and not consulted when a lease is placed.
#[tokio::test]
async fn a_cordoned_host_takes_no_new_lease_until_it_is_uncordoned() {
    let h = start_agents("cordon_placement", false).await;
    let me = enrolled(&h, "burst-1", "i-1").await;
    let placed = cordon(&h, "burst-1", &me, 60).await;
    assert_eq!(placed.status, 200, "{}", placed.body);
    let body = placed.json();
    assert_eq!(body["already_cordoned"], false);
    assert_eq!(body["cordon"]["expires_at_ms"], T0 + 60_000);
    assert_eq!(body["cordon"]["expires_in_ms"], 60_000);
    assert!(no_capacity(&lease(&h).await));

    let lifted = uncordon(&h, "burst-1", &me).await;
    assert_eq!(lifted.status, 200, "{}", lifted.body);
    assert_eq!(lifted.json()["lifted"], true);
    assert_eq!(lease(&h).await.json()["host"], "burst-1");
}

/// Asserts that a cordon cuts nothing: a lease held when the host is cordoned stays held
/// and can be renewed, the host's read counts it until it is released, and after the
/// release the host still takes no new lease. It catches a cordon that gives up the lease
/// it finds, and one that lets the freed host be placed again.
#[tokio::test]
async fn a_lease_held_before_the_cordon_runs_on_and_its_end_leaves_the_host_cordoned() {
    let h = start_agents("cordon_held", false).await;
    let me = enrolled(&h, "burst-1", "i-1").await;
    assert_eq!(lease(&h).await.json()["id"], "1-1");
    assert_eq!(cordon(&h, "burst-1", &me, 600).await.status, 200);
    let busy = show(&h, "burst-1", &me).await;
    assert_eq!(
        (busy["leases"].clone(), busy["lease"].clone()),
        (json!(1), json!("1-1"))
    );
    assert_eq!(
        h.call("POST", "/v1/leases/1-1/renew", None).await.status,
        200
    );

    assert_eq!(h.call("DELETE", "/v1/leases/1-1", None).await.status, 200);
    live_for(&h, &me, 10).await;
    let idle = show(&h, "burst-1", &me).await;
    assert_eq!(idle["leases"], 0);
    assert!(idle.get("lease").is_none(), "{idle}");
    assert_eq!(idle["releasing"], false);
    assert!(no_capacity(&lease(&h).await));
}

/// Asserts the lease of a cordon: a request with no length gets the default; a repeat
/// extends the expiry from the time of the repeat and says the host was cordoned already;
/// a shorter repeat leaves the expiry where it was; a length over the server's maximum is
/// cut to it; and a length of zero, a body that is not a request and a field the request
/// does not have are 400 `invalid-cordon`, changing nothing. It catches a renew that
/// shortens, a cap that is not applied, and a body that is half read.
#[tokio::test]
async fn a_renew_extends_the_cordon_and_the_server_caps_it() {
    let h = start_agents("cordon_renew", false).await;
    let me = enrolled(&h, "burst-1", "i-1").await;
    let first = own(&h, "POST", "burst-1", Some("cordon"), &me.credential, None).await;
    assert_eq!(first.status, 200, "{}", first.body);
    assert_eq!(
        first.json()["cordon"]["expires_at_ms"],
        T0 + DEFAULT_SECS * 1000
    );

    live_for(&h, &me, 30).await;
    let renewed = cordon(&h, "burst-1", &me, 60).await.json();
    assert_eq!(renewed["already_cordoned"], true);
    assert_eq!(renewed["cordon"]["expires_at_ms"], T0 + DEFAULT_SECS * 1000);
    let renewed = cordon(&h, "burst-1", &me, 290).await.json();
    assert_eq!(renewed["cordon"]["expires_at_ms"], T0 + 30_000 + 290_000);
    assert_eq!(renewed["cordon"]["expires_in_ms"], 290_000);
    let shorter = cordon(&h, "burst-1", &me, 10).await.json();
    assert_eq!(shorter["cordon"]["expires_at_ms"], T0 + 30_000 + 290_000);

    let capped = cordon(&h, "burst-1", &me, MAX_SECS * 1000).await.json();
    assert_eq!(capped["cordon"]["expires_in_ms"], MAX_SECS * 1000);
    for bad in [
        Some(json!({ "lease_secs": 0 })),
        Some(json!({ "lease_secs": "soon" })),
        Some(json!({ "lease_secs": 5, "extra": true })),
    ] {
        let refused = own(
            &h,
            "POST",
            "burst-1",
            Some("cordon"),
            &me.credential,
            bad.as_ref(),
        )
        .await;
        assert_eq!(refused.status, 400, "{}", refused.body);
        assert_eq!(code(&refused), "invalid-cordon");
    }
    let unread = request(
        h.addr(),
        "POST",
        &path("burst-1", Some("cordon")),
        Some(&me.credential),
        Some(b"not json"),
    )
    .await;
    assert_eq!(
        (unread.status, code(&unread)),
        (400, "invalid-cordon".into())
    );
    let after = show(&h, "burst-1", &me).await;
    assert_eq!(after["cordon"]["expires_in_ms"], MAX_SECS * 1000);
}

/// Asserts that a cordon nobody renews lifts by itself: just before its expiry the host
/// takes no lease, at it the host does, and the read no longer shows a cordon. The host
/// heartbeats throughout, so it is live. It catches an expiry that is never read, and one
/// that lifts early.
#[tokio::test]
async fn a_cordon_that_is_not_renewed_lifts_by_itself() {
    let h = start_agents("cordon_expiry", false).await;
    let me = enrolled(&h, "burst-1", "i-1").await;
    assert_eq!(cordon(&h, "burst-1", &me, 60).await.status, 200);
    live_for(&h, &me, 55).await;
    assert!(no_capacity(&lease(&h).await));
    assert!(show(&h, "burst-1", &me).await.get("cordon").is_some());

    live_for(&h, &me, 10).await;
    let shown = show(&h, "burst-1", &me).await;
    assert!(shown.get("cordon").is_none(), "{shown}");
    assert_eq!(lease(&h).await.json()["host"], "burst-1");
}

/// Asserts that an uncordon changes nothing it should not: on a host that is not cordoned
/// it is 200 and says it lifted nothing, after a cordon it lifts and says so, and a second
/// one is 200 again. It catches an uncordon that is an error when there is nothing to lift,
/// which a hook that retries cannot use.
#[tokio::test]
async fn an_uncordon_is_idempotent() {
    let h = start_agents("cordon_uncordon", false).await;
    let me = enrolled(&h, "burst-1", "i-1").await;
    let none = uncordon(&h, "burst-1", &me).await;
    assert_eq!(
        (none.status, none.json()["lifted"].clone()),
        (200, json!(false))
    );
    assert_eq!(cordon(&h, "burst-1", &me, 60).await.status, 200);
    let lifted = uncordon(&h, "burst-1", &me).await;
    assert_eq!(
        (lifted.status, lifted.json()["lifted"].clone()),
        (200, json!(true))
    );
    let again = uncordon(&h, "burst-1", &me).await;
    assert_eq!(
        (again.status, again.json()["lifted"].clone()),
        (200, json!(false))
    );
    assert!(show(&h, "burst-1", &me).await.get("cordon").is_none());
}

/// Asserts what the read of a host says: its id, session state, the agent version and the
/// runtime its hello reported, no lease, no cordon and no taint for a clean host; then,
/// after a lease and a cordon and a taint report, the lease count and id, the cordon and
/// the taint. It catches a field the update hook waits on that is missing or wrong.
#[tokio::test]
async fn the_read_of_a_host_has_what_the_update_hook_waits_on() {
    let h = start_agents("cordon_read", false).await;
    let me = enrolled(&h, "burst-1", "i-1").await;
    let clean = show(&h, "burst-1", &me).await;
    assert_eq!(
        clean,
        json!({
            "id": "burst-1",
            "state": "live",
            "leases": 0,
            "releasing": false,
            "agent_version": "0.1.0",
            "runtime": "container",
        })
    );

    assert_eq!(lease(&h).await.json()["id"], "1-1");
    assert_eq!(cordon(&h, "burst-1", &me, 120).await.status, 200);
    let mut beat = me.session.beat();
    beat["taint"] = json!({ "reason": "residue", "run_id": "run-1-1", "check": "tmp" });
    let reported = post(&h, "/v1/agent/heartbeat", Some(&me.credential), &beat).await;
    assert_eq!(reported.status, 200, "{}", reported.body);
    let full = show(&h, "burst-1", &me).await;
    assert_eq!(
        full,
        json!({
            "id": "burst-1",
            "state": "live",
            "leases": 1,
            "lease": "1-1",
            "releasing": false,
            "cordon": { "expires_at_ms": T0 + 120_000, "expires_in_ms": 120_000 },
            "taint": { "reason": "residue", "run_id": "run-1-1", "check": "tmp" },
            "agent_version": "0.1.0",
            "runtime": "container",
        })
    );
}

/// Asserts the routes that are not routes: an unknown verb, a verb on a read, and a post
/// with no verb are 404 `not-found`, for a host that is the caller's own, and change
/// nothing. It catches a verb that falls through to the read or to the cordon.
#[tokio::test]
async fn a_path_that_names_no_action_is_not_found() {
    let h = start_agents("cordon_paths", false).await;
    let me = enrolled(&h, "burst-1", "i-1").await;
    let calls = [
        ("POST", Some("drain")),
        ("POST", None),
        ("GET", Some("cordon")),
        ("GET", Some("drain")),
    ];
    for (method, verb) in calls {
        let reply = own(&h, method, "burst-1", verb, &me.credential, None).await;
        assert_eq!(reply.status, 404, "{method} {verb:?}: {}", reply.body);
        assert_eq!(code(&reply), "not-found");
    }
    assert!(show(&h, "burst-1", &me).await.get("cordon").is_none());
}

/// Asserts that a cordon is kept in the agent state file and survives a restart: after it
/// the host (which holds no lease) is read with the same expiry, still awaiting its agent,
/// takes no lease once the agent says hello again, and is lifted at the expiry as before.
/// It catches a cordon held in memory only, and one a hello forgets.
#[tokio::test]
async fn a_cordon_survives_a_restart() {
    let first = start_agents("cordon_restart", false).await;
    let me = enrolled(&first, "burst-1", "i-1").await;
    assert_eq!(cordon(&first, "burst-1", &me, 120).await.status, 200);
    let mut file = first.scratch.state().into_os_string();
    file.push(".agents");
    let text = fs::read_to_string(&file).unwrap();
    assert!(text.contains("cordon_until_ms = 1120000"), "{text}");

    let h = restarted(first, false).await;
    let read = show(&h, "burst-1", &me).await;
    assert_eq!(read["state"], "awaiting");
    assert_eq!(read["cordon"]["expires_at_ms"], T0 + 120_000);
    let back = post(
        &h,
        "/v1/agent/hello",
        Some(&me.credential),
        &asking("burst-1", "i-1"),
    )
    .await;
    let me = Host {
        session: session(&back, "burst-1"),
        ..me
    };
    assert_eq!(show(&h, "burst-1", &me).await["state"], "live");
    assert!(no_capacity(&lease(&h).await));

    live_for(&h, &me, 125).await;
    assert!(show(&h, "burst-1", &me).await.get("cordon").is_none());
    assert_eq!(lease(&h).await.json()["host"], "burst-1");
}

/// Asserts that a lifted cordon is not brought back by a restart, and that a host the
/// registry does not hold (its server restarted, it holds no lease, is not tainted and
/// is not cordoned) is 404 `unknown-host` on all three routes until its agent says hello
/// again. It catches an uncordon that leaves the row, and a route that makes up a host.
#[tokio::test]
async fn a_lifted_cordon_stays_lifted_and_an_unknown_host_is_not_found() {
    let first = start_agents("cordon_lifted_restart", false).await;
    let me = enrolled(&first, "burst-1", "i-1").await;
    assert_eq!(cordon(&first, "burst-1", &me, 120).await.status, 200);
    assert_eq!(uncordon(&first, "burst-1", &me).await.status, 200);
    let h = restarted(first, false).await;
    for reply in [
        own(&h, "GET", "burst-1", None, &me.credential, None).await,
        cordon(&h, "burst-1", &me, 60).await,
        uncordon(&h, "burst-1", &me).await,
    ] {
        assert_eq!(reply.status, 404, "{}", reply.body);
        assert_eq!(code(&reply), "unknown-host");
    }
}

/// Asserts that a cordon that cannot be written down is not acknowledged: with the state
/// file blocked the cordon answers 503 `cordon-unsaved` (and holds on this server, so the
/// host takes no lease), the failure is counted, and the same call after the file can be
/// written is 200 and in the file; an uncordon that cannot be written is 503 too and is
/// repeated with the same result. It catches an acknowledgement for a cordon a restart
/// would lose, and an uncordon that says it is done when the file still has the cordon.
#[tokio::test]
async fn a_cordon_that_cannot_be_written_is_not_acknowledged() {
    let h = start_agents("cordon_unsaved", false).await;
    let me = enrolled(&h, "burst-1", "i-1").await;
    let blocker = h.scratch.path("leases.toml.agents.tmp");
    fs::create_dir(&blocker).unwrap();
    let errors = h.running.agent_errors();

    let refused = cordon(&h, "burst-1", &me, 60).await;
    assert_eq!(refused.status, 503, "{}", refused.body);
    assert_eq!(code(&refused), "cordon-unsaved");
    assert_eq!(h.running.agent_errors(), errors + 1);
    assert!(no_capacity(&lease(&h).await));

    fs::remove_dir(&blocker).unwrap();
    assert_eq!(cordon(&h, "burst-1", &me, 60).await.status, 200);
    let mut file = h.scratch.state().into_os_string();
    file.push(".agents");
    assert!(
        fs::read_to_string(&file)
            .unwrap()
            .contains("cordon_until_ms")
    );

    fs::create_dir(&blocker).unwrap();
    let stuck = uncordon(&h, "burst-1", &me).await;
    assert_eq!((stuck.status, code(&stuck)), (503, "cordon-unsaved".into()));
    fs::remove_dir(&blocker).unwrap();
    let done = uncordon(&h, "burst-1", &me).await;
    assert_eq!(done.status, 200, "{}", done.body);
    assert!(
        !fs::read_to_string(&file)
            .unwrap()
            .contains("cordon_until_ms")
    );
}
