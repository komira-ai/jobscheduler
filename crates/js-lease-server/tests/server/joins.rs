//! One-time join tokens: minted for a launched instance behind the client token, used
//! once to register one host, kept across a restart, and never shown again.

use std::fs;
use std::time::Duration;

use js_lease::Clock;
use serde_json::{Value, json};

use js_lease_server::{SharedClock, channel_ticker};

use crate::agents_support::{
    JOIN, Session, agent_args, hello_body, hello_with, hosts, listed, post, restarted, session,
    start_agents,
};
use crate::support::{Harness, LogCapture, Reply, TOKEN, config, replace, request, start};

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

pub const MINT: &str = "/v1/agent/join-tokens";

/// `POST` of the mint route with the client token.
pub async fn mint(h: &Harness, instance: &str) -> Reply {
    let body = json!({ "provider": "lambdalabs", "instance_id": instance });
    h.call("POST", MINT, Some(&body)).await
}

/// A token minted for `instance`, which must succeed.
pub async fn minted(h: &Harness, instance: &str) -> String {
    let reply = mint(h, instance).await;
    assert_eq!(reply.status, 201, "{}", reply.body);
    reply.json()["token"].as_str().unwrap().to_string()
}

/// A token minted for `instance` that lasts `ttl_seconds`; its expiry is checked.
async fn minted_for(h: &Harness, instance: &str, ttl_seconds: u64) -> String {
    let body =
        json!({ "provider": "lambdalabs", "instance_id": instance, "ttl_seconds": ttl_seconds });
    let reply = h.call("POST", MINT, Some(&body)).await;
    assert_eq!(reply.status, 201, "{}", reply.body);
    let expiry = h.clock.now().millis() + ttl_seconds * 1000;
    assert_eq!(reply.json()["expires_at_ms"].as_u64(), Some(expiry));
    reply.json()["token"].as_str().unwrap().to_string()
}

/// The hello of `host` that says it runs on `instance`.
fn hello_on(host: &str, instance: &str) -> Value {
    let mut body = hello_body(host);
    body["instance"] = json!({ "provider": "lambdalabs", "instance_id": instance });
    body
}

/// A hello with `token`.
pub async fn hello(h: &Harness, token: &str, host: &str, instance: &str) -> Reply {
    post(h, "/v1/agent/hello", Some(token), &hello_on(host, instance)).await
}

/// The code of a refusal.
pub fn code(reply: &Reply) -> String {
    reply.json()["error"].as_str().unwrap().to_string()
}

/// A heartbeat of `session` with `token`.
pub async fn beat_with(h: &Harness, token: &str, session: &Session) -> Reply {
    post(h, "/v1/agent/heartbeat", Some(token), &session.beat()).await
}

/// Asserts that a token minted for an instance registers the host that names that
/// instance, that the host then heartbeats and is listed, and that the answer to the mint
/// is a 201 with a 64-digit token and the expiry the clock and the default give. It
/// catches a token that does not admit, one that admits but cannot carry the session, and
/// a wrong expiry.
#[tokio::test]
async fn a_minted_token_registers_a_host() {
    let h = start_agents("joins_register", false).await;
    let reply = mint(&h, "i-1").await;
    assert_eq!(reply.status, 201, "{}", reply.body);
    let answer = reply.json();
    let token = answer["token"].as_str().unwrap();
    assert_eq!(token.len(), 64);
    assert!(token.bytes().all(|b| b.is_ascii_hexdigit()));
    assert_eq!(
        answer["expires_at_ms"].as_u64(),
        Some(crate::support::T0 + 30 * 60 * 1000)
    );
    let welcome = hello(&h, token, "burst-1", "i-1").await;
    let session = session(&welcome, "burst-1");
    assert_eq!(beat_with(&h, token, &session).await.status, 200);
    assert_eq!(listed(&hosts(&h).await), ["burst-1"]);
}

/// Asserts that a token that registered a host admits no other host id, with the code
/// `join-token-used`, and cannot heartbeat for one; and that the host it registered says
/// hello again with it after the session is lost. It catches a token that becomes a pool
/// token after its first use, and one that locks its own host out.
#[tokio::test]
async fn a_second_use_by_another_host_is_refused() {
    let h = start_agents("joins_second_use", false).await;
    let token = minted(&h, "i-1").await;
    let first = session(&hello(&h, &token, "burst-1", "i-1").await, "burst-1");
    let second = hello(&h, &token, "burst-2", "i-1").await;
    assert_eq!(
        (second.status, code(&second)),
        (403, "join-token-used".into())
    );
    let stolen = Session {
        host: "burst-2".into(),
        ..first.clone()
    };
    let beat = beat_with(&h, &token, &stolen).await;
    assert_eq!(
        (beat.status, code(&beat)),
        (403, "join-token-not-admitted".into())
    );
    assert_eq!(beat_with(&h, &token, &first).await.status, 200);
    h.clock.advance(secs(60));
    let again = hello(&h, &token, "burst-1", "i-1").await;
    assert_eq!(again.status, 200, "{}", again.body);
}

/// Asserts that an unused token is refused from its expiry on with `join-token-expired`,
/// and that a token used before its expiry still carries its host's heartbeats after it.
/// It catches a token that never expires and an expiry that cuts off a registered host.
#[tokio::test]
async fn an_expired_token_is_refused() {
    let h = start_agents("joins_expired", false).await;
    let kept = minted_for(&h, "i-1", 10).await;
    let unused = minted_for(&h, "i-2", 10).await;
    let held = session(&hello(&h, &kept, "burst-1", "i-1").await, "burst-1");
    h.clock.advance(secs(10));
    let late = hello(&h, &unused, "burst-2", "i-2").await;
    assert_eq!(
        (late.status, code(&late)),
        (403, "join-token-expired".into())
    );
    assert_eq!(beat_with(&h, &kept, &held).await.status, 200);
}

/// Asserts that a hello that names another instance, or none, is refused with
/// `join-token-instance` and does not consume the token: the right instance still
/// registers. It catches a token that admits any instance and one that a wrong claim
/// burns.
#[tokio::test]
async fn a_wrong_instance_is_refused_and_the_token_stays_good() {
    let h = start_agents("joins_instance", false).await;
    let token = minted(&h, "i-1").await;
    let other = hello(&h, &token, "burst-1", "i-2").await;
    assert_eq!(
        (other.status, code(&other)),
        (403, "join-token-instance".into())
    );
    let plain = post(&h, "/v1/agent/hello", Some(&token), &hello_body("burst-1")).await;
    assert_eq!(
        (plain.status, code(&plain)),
        (403, "join-token-instance".into())
    );
    assert_eq!(hello(&h, &token, "burst-1", "i-1").await.status, 200);
}

/// Asserts that a heartbeat with a token that has registered nobody is refused with
/// `join-token-not-admitted`. It catches a token that is a pool token until its first
/// hello.
#[tokio::test]
async fn an_unused_token_cannot_heartbeat() {
    let h = start_agents("joins_unused_beat", false).await;
    let token = minted(&h, "i-1").await;
    let beat = Session {
        host: "burst-1".into(),
        epoch: 1,
        secret: "x".into(),
    };
    let reply = beat_with(&h, &token, &beat).await;
    assert_eq!(
        (reply.status, code(&reply)),
        (403, "join-token-not-admitted".into())
    );
}

/// Asserts that the token shows only in the answer to the mint: no other answer, no log
/// line, no file the server keeps holds it, including the refusals. It catches a refusal
/// that echoes the credential, a log line that names it, and a file that stores it.
#[tokio::test]
async fn the_token_is_shown_once_and_stored_never() {
    let logs = LogCapture::start();
    let h = start_agents("joins_secret", false).await;
    let token = minted(&h, "i-1").await;
    let mut answers = vec![hello(&h, &token, "burst-1", "i-2").await];
    let welcome = hello(&h, &token, "burst-1", "i-1").await;
    let held = session(&welcome, "burst-1");
    answers.push(welcome);
    answers.push(hello(&h, &token, "burst-2", "i-1").await);
    answers.push(beat_with(&h, &token, &held).await);
    answers.push(hello(&h, &format!("{token}x"), "burst-2", "i-1").await);
    h.ticks.step().await;
    for answer in &answers {
        assert!(!answer.body.contains(&token), "{}", answer.body);
    }
    let text = logs.text();
    assert!(text.contains("join token minted"), "{text}");
    assert!(!text.contains(&token), "{text}");
    for entry in fs::read_dir(&h.scratch.dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_file() {
            let held = String::from_utf8_lossy(&fs::read(&path).unwrap()).into_owned();
            assert!(!held.contains(&token), "{}", path.display());
        }
    }
    let mut joins = h.scratch.state().into_os_string();
    joins.push(".joins");
    let joins = fs::read_to_string(joins).unwrap();
    assert!(joins.contains("used_by = \"burst-1\""), "{joins}");
}

/// Asserts that the mint route takes the client token only: no token, a wrong one, and a
/// pool join token (which is not the client token) are each a 401 that says nothing; and
/// that a body that is not a request is a 400 and mints nothing. It catches a route that
/// lets a holder of a pool join token, or anyone, make tokens.
#[tokio::test]
async fn minting_needs_the_client_token() {
    let h = start_agents("joins_auth", false).await;
    let body = json!({ "provider": "lambdalabs", "instance_id": "i-1" })
        .to_string()
        .into_bytes();
    for token in [
        None,
        Some("wrong-token-0123456789-abcdefghijklmnop"),
        Some(JOIN),
    ] {
        let reply = request(h.addr(), "POST", MINT, token, Some(&body)).await;
        assert_eq!(reply.status, 401, "{token:?}");
        assert!(reply.body.is_empty(), "{}", reply.body);
    }
    let bad = [
        json!({ "provider": "lambdalabs" }),
        json!({ "provider": "Bad Provider", "instance_id": "i-1" }),
        json!({ "provider": "lambdalabs", "instance_id": "i 1" }),
        json!({ "provider": "lambdalabs", "instance_id": "i-1", "ttl_seconds": 0 }),
        json!({ "provider": "lambdalabs", "instance_id": "i-1", "ttl_seconds": 86401 }),
        json!({ "provider": "lambdalabs", "instance_id": "i-1", "extra": 1 }),
    ];
    for body in bad {
        let reply = h.call("POST", MINT, Some(&body)).await;
        assert_eq!(reply.status, 400, "{body}: {}", reply.body);
    }
    let unused = hello(
        &h,
        "no-such-token-0123456789-abcdefghijkl",
        "burst-1",
        "i-1",
    )
    .await;
    assert_eq!(unused.status, 401);
}

/// Asserts that a server whose file of tokens cannot be written makes no token: a 500
/// `state-write-failed`, and nothing it said afterwards admits a host. It catches a
/// token that is handed out although a restart would forget it.
#[tokio::test]
async fn a_mint_the_disk_refuses_makes_no_token() {
    let h = start_agents("joins_unwritable", false).await;
    let mut joins = h.scratch.state().into_os_string();
    joins.push(".joins");
    fs::create_dir(&joins).unwrap();
    let reply = mint(&h, "i-1").await;
    assert_eq!(reply.status, 500, "{}", reply.body);
    assert_eq!(code(&reply), "state-write-failed");
    assert!(!reply.body.contains("token\":"), "{}", reply.body);
}

/// Asserts that the route exists only when agents register hosts, and that a client token
/// is all it takes there. It catches a route that is open on a server with no agents.
#[tokio::test]
async fn there_is_no_mint_route_without_agents() {
    let h = Harness::new("joins_no_agents").await;
    let reply = mint(&h, "i-1").await;
    assert_eq!(reply.status, 404, "{}", reply.body);
    let plain = request(h.addr(), "POST", MINT, Some(TOKEN), Some(b"{}")).await;
    assert_eq!(plain.status, 404);
}

/// Asserts that a second token for an instance replaces the first, which then
/// authenticates nothing. It catches two live ways into one instance after a delivery
/// that failed and was retried.
#[tokio::test]
async fn a_new_token_for_an_instance_replaces_the_unused_one() {
    let h = start_agents("joins_replace", false).await;
    let first = minted(&h, "i-1").await;
    let second = minted(&h, "i-1").await;
    assert_ne!(first, second);
    assert_eq!(hello(&h, &first, "burst-1", "i-1").await.status, 401);
    assert_eq!(hello(&h, &second, "burst-1", "i-1").await.status, 200);
}

/// Asserts what a restart keeps: the token that was not used admits its host, the one
/// that was used still refuses another host and still lets its own host say hello again.
/// It catches a restart that gives a used token back (a second host registers) and one
/// that drops a token nobody used yet.
#[tokio::test]
async fn a_restart_keeps_an_unused_token_and_a_used_one() {
    let first = start_agents("joins_restart", false).await;
    let used = minted(&first, "i-1").await;
    let unused = minted(&first, "i-2").await;
    assert_eq!(hello(&first, &used, "burst-1", "i-1").await.status, 200);

    let h = restarted(first, false).await;
    let stolen = hello(&h, &used, "burst-2", "i-1").await;
    assert_eq!(
        (stolen.status, code(&stolen)),
        (403, "join-token-used".into())
    );
    let own = hello(&h, &used, "burst-1", "i-1").await;
    assert_eq!(own.status, 200, "{}", own.body);
    let late = hello(&h, &unused, "burst-2", "i-2").await;
    assert_eq!(late.status, 200, "{}", late.body);
}

/// Asserts that of two hellos racing for one unused token, for two host ids, exactly one
/// is admitted and the other is refused as used. It catches a check and a use that are not
/// one step, which lets both register.
#[tokio::test]
async fn of_two_concurrent_uses_one_wins() {
    let h = start_agents("joins_race", false).await;
    let token = minted(&h, "i-1").await;
    let (one, two) = tokio::join!(
        hello(&h, &token, "burst-1", "i-1"),
        hello(&h, &token, "burst-2", "i-1")
    );
    let mut statuses = [one.status, two.status];
    statuses.sort_unstable();
    assert_eq!(statuses, [200, 403], "{} / {}", one.body, two.body);
    let refused = if one.status == 403 { &one } else { &two };
    assert_eq!(code(refused), "join-token-used");
}

/// Asserts that a hello the registry refuses leaves the one-time token unused: the host
/// id is bound to the pool's join token, so the hello with the one-time token is a 403
/// `host-bound`, and the same token then registers another host id. It catches a token that
/// is spent by a hello that registered nobody, which would leave the launched instance
/// without a way in.
#[tokio::test]
async fn a_hello_the_registry_refuses_does_not_spend_the_token() {
    let h = start_agents("joins_rollback", false).await;
    assert_eq!(hello_with(&h, JOIN, "burst-1").await.status, 200);
    let token = minted(&h, "i-1").await;
    let refused = hello(&h, &token, "burst-1", "i-1").await;
    assert_eq!((refused.status, code(&refused)), (403, "host-bound".into()));
    let mut joins = h.scratch.state().into_os_string();
    joins.push(".joins");
    let text = fs::read_to_string(&joins).unwrap();
    assert!(!text.contains("used_by"), "{text}");
    let later = hello(&h, &token, "burst-2", "i-1").await;
    assert_eq!(later.status, 200, "{}", later.body);
}

/// Asserts that a used token stops being a credential once its host has been silent for a
/// day: the hello and the heartbeat are 403 `join-token-expired`, the next mint drops the
/// entry from the file, and the digest is then unknown (401). It catches a used token that
/// stays a way to register its host id for ever.
#[tokio::test]
async fn a_used_token_ends_after_a_day_of_silence() {
    let h = start_agents("joins_credential_bound", false).await;
    let token = minted(&h, "i-1").await;
    let held = session(&hello(&h, &token, "burst-1", "i-1").await, "burst-1");
    h.clock.advance(secs(86_400));
    for refused in [
        hello(&h, &token, "burst-1", "i-1").await,
        beat_with(&h, &token, &held).await,
    ] {
        assert_eq!(
            (refused.status, code(&refused)),
            (403, "join-token-expired".into())
        );
    }
    minted(&h, "i-2").await;
    let mut joins = h.scratch.state().into_os_string();
    joins.push(".joins");
    let text = fs::read_to_string(&joins).unwrap();
    assert!(!text.contains("burst-1"), "{text}");
    assert_eq!(hello(&h, &token, "burst-1", "i-1").await.status, 401);
}

/// Asserts that a heartbeat just before the day is up renews it: a token whose host
/// beats at 23h59m59s, and again just before the renewed day is up, is still not refused as
/// expired, which it would be on the first day's bound. It catches a bound that is not
/// renewed by use.
#[tokio::test]
async fn a_heartbeat_renews_the_credential() {
    let h = start_agents("joins_credential_renewed", false).await;
    let token = minted(&h, "i-1").await;
    let held = session(&hello(&h, &token, "burst-1", "i-1").await, "burst-1");
    h.clock.advance(secs(86_399));
    let first = beat_with(&h, &token, &held).await;
    assert_eq!(first.status, 200, "{}", first.body);
    h.clock.advance(secs(86_399));
    let second = beat_with(&h, &token, &held).await;
    assert_eq!(second.status, 200, "{}", second.body);
}

/// Asserts that a second server over the state file of a running one is refused at start,
/// naming `--state-file`, and so cannot hand a used token back. It catches a start that
/// reads the file of tokens and serves while another process owns it.
#[tokio::test]
async fn a_second_server_over_the_same_state_is_refused() {
    let h = start_agents("joins_second_server", false).await;
    let token = minted(&h, "i-1").await;
    assert_eq!(hello(&h, &token, "burst-1", "i-1").await.status, 200);
    let args = replace(agent_args(&h.scratch, false), "--boot", "resume");
    let (_, ticker) = channel_ticker();
    let clock = SharedClock::new(js_lease::ManualClock::default());
    let started = start(config(args).unwrap(), clock, ticker).await;
    let refusal = started.err().expect("the second start should be refused");
    assert_eq!(refusal.flag(), "--state-file");
    assert!(refusal.reason().contains("lock"), "{}", refusal.reason());
}
