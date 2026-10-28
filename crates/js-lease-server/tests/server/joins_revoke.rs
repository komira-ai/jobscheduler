//! Revoking the one-time tokens of an instance (`DELETE /v1/agent/join-tokens`), which the
//! scheduler's terminate path calls, and the headers of the mint's answer.

use std::fs;

use serde_json::{Value, json};

use crate::agents_support::{JOIN, hosts, listed, restarted, session, start_agents};
use crate::joins::{MINT, beat_with, code, hello, minted};
use crate::support::{Harness, LogCapture, Reply, TOKEN, request};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// `DELETE` of the revoke route with the client token.
async fn revoke(h: &Harness, instance: &str) -> Reply {
    let body = json!({ "provider": "lambdalabs", "instance_id": instance });
    h.call("DELETE", MINT, Some(&body)).await
}

fn joins_file(h: &Harness) -> std::path::PathBuf {
    let mut joins = h.scratch.state().into_os_string();
    joins.push(".joins");
    joins.into()
}

/// Asserts that revoking a token that registered a host answers 204 with no body, and
/// that the token then gets the 401 of an unknown token on a heartbeat and on a new
/// hello of the same host, also after a restart; that the file holds no row for it; and
/// that the host it registered stays listed (the registry's own retention ends that),
/// and a token minted afterwards for the instance works. It catches a used token that
/// stays a credential after its instance is gone, and a revoke kept in memory only.
#[tokio::test]
async fn a_revoked_used_token_is_refused() {
    let h = start_agents("joins_revoke_used", false).await;
    let token = minted(&h, "i-1").await;
    let held = session(&hello(&h, &token, "burst-1", "i-1").await, "burst-1");
    assert_eq!(beat_with(&h, &token, &held).await.status, 200);
    let reply = revoke(&h, "i-1").await;
    assert_eq!((reply.status, reply.body.as_str()), (204, ""));
    for refused in [
        beat_with(&h, &token, &held).await,
        hello(&h, &token, "burst-1", "i-1").await,
    ] {
        assert_eq!(refused.status, 401, "{}", refused.body);
        assert!(refused.body.is_empty(), "{}", refused.body);
    }
    assert!(
        !fs::read_to_string(joins_file(&h))
            .unwrap()
            .contains("burst-1")
    );
    assert_eq!(listed(&hosts(&h).await), ["burst-1"]);

    let h = restarted(h, false).await;
    assert_eq!(hello(&h, &token, "burst-1", "i-1").await.status, 401);
    let fresh = minted(&h, "i-1").await;
    assert_eq!(hello(&h, &fresh, "burst-2", "i-1").await.status, 200);
}

/// Asserts that revoking a token nobody used stops its first hello (401), and that the
/// token of another instance is not touched. It catches a revoke that only handles used
/// tokens, and one that drops every token.
#[tokio::test]
async fn a_revoked_unused_token_cannot_say_hello() {
    let h = start_agents("joins_revoke_unused", false).await;
    let gone = minted(&h, "i-1").await;
    let kept = minted(&h, "i-2").await;
    assert_eq!(revoke(&h, "i-1").await.status, 204);
    assert_eq!(hello(&h, &gone, "burst-1", "i-1").await.status, 401);
    assert_eq!(hello(&h, &kept, "burst-2", "i-2").await.status, 200);
}

/// Asserts that the revoke route takes the client token only: no token, a wrong one and a
/// pool join token are each a 401 with no body, and the token is still good after them;
/// and that a body that is not a request is a 400. It catches a route that lets a holder
/// of a pool join token, or anyone, revoke the credentials of other hosts.
#[tokio::test]
async fn revoking_needs_the_client_token() {
    let h = start_agents("joins_revoke_auth", false).await;
    let token = minted(&h, "i-1").await;
    let body = json!({ "provider": "lambdalabs", "instance_id": "i-1" })
        .to_string()
        .into_bytes();
    for given in [
        None,
        Some("wrong-token-0123456789-abcdefghijklmnop"),
        Some(JOIN),
        Some(token.as_str()),
    ] {
        let reply = request(h.addr(), "DELETE", MINT, given, Some(&body)).await;
        assert_eq!(reply.status, 401, "{given:?}");
        assert!(reply.body.is_empty(), "{}", reply.body);
    }
    assert_eq!(hello(&h, &token, "burst-1", "i-1").await.status, 200);
    let bad = [
        json!({ "provider": "lambdalabs" }),
        json!({ "provider": "Bad Provider", "instance_id": "i-1" }),
        json!({ "provider": "lambdalabs", "instance_id": "i 1" }),
        json!({ "provider": "lambdalabs", "instance_id": "i-1", "extra": 1 }),
    ];
    for body in bad {
        let reply = h.call("DELETE", MINT, Some(&body)).await;
        assert_eq!(reply.status, 400, "{body}: {}", reply.body);
    }
}

/// Asserts that revoking is idempotent: a second revoke and a revoke of an instance that
/// was never minted are each a 204 with no body, and the file's contents stay as they were.
/// (That no write happens is proved at the store, where the file is unwritable.) It
/// catches a terminate that has to treat a retry specially.
#[tokio::test]
async fn revoking_twice_or_an_unknown_instance_is_204() {
    let h = start_agents("joins_revoke_idempotent", false).await;
    minted(&h, "i-1").await;
    assert_eq!(revoke(&h, "i-1").await.status, 204);
    let written = fs::read_to_string(joins_file(&h)).unwrap();
    for instance in ["i-1", "never-minted"] {
        let again = revoke(&h, instance).await;
        assert_eq!((again.status, again.body.as_str()), (204, ""));
    }
    assert_eq!(fs::read_to_string(joins_file(&h)).unwrap(), written);
}

/// Asserts that a revoke the disk refuses is a 503 `state-write-failed` and the token is
/// as it was: the host it registered still heartbeats. It catches a token dropped in
/// memory that a restart would give back.
#[tokio::test]
async fn a_revoke_the_disk_refuses_leaves_the_token() {
    let h = start_agents("joins_revoke_unwritable", false).await;
    let token = minted(&h, "i-1").await;
    let held = session(&hello(&h, &token, "burst-1", "i-1").await, "burst-1");
    let path = joins_file(&h);
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    let reply = revoke(&h, "i-1").await;
    assert_eq!(reply.status, 503, "{}", reply.body);
    assert_eq!(code(&reply), "state-write-failed");
    assert_eq!(beat_with(&h, &token, &held).await.status, 200);
}

/// Asserts that the token is not in the revoke's answer or in the log lines it writes, and
/// that the log says which instance was revoked. It catches a log line or answer that
/// echoes a credential.
#[tokio::test]
async fn the_revoke_log_and_reply_never_hold_the_token() {
    let logs = LogCapture::start();
    let h = start_agents("joins_revoke_secret", false).await;
    let token = minted(&h, "i-1").await;
    let reply = revoke(&h, "i-1").await;
    assert!(!reply.body.contains(&token), "{}", reply.body);
    let text = logs.text();
    assert!(text.contains("join tokens revoked"), "{text}");
    assert!(!text.contains(&token), "{text}");
}

/// Asserts that the 201 that carries a token has `Cache-Control: no-store`. It catches a
/// cache on the way that keeps the only answer that shows the token.
#[tokio::test]
async fn the_mint_answer_is_not_stored() {
    let h = start_agents("joins_mint_no_store", false).await;
    let body = json!({ "provider": "lambdalabs", "instance_id": "i-1" }).to_string();
    let head = format!(
        "POST {MINT} HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {TOKEN}\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let mut stream = TcpStream::connect(h.addr()).await.unwrap();
    stream.write_all(head.as_bytes()).await.unwrap();
    let mut bytes = Vec::new();
    let _ = stream.read_to_end(&mut bytes).await;
    let text = String::from_utf8_lossy(&bytes).to_ascii_lowercase();
    let (head, reply) = text.split_once("\r\n\r\n").unwrap();
    assert!(head.starts_with("http/1.1 201"), "{head}");
    assert!(head.contains("cache-control: no-store"), "{head}");
    let _: Value = serde_json::from_str(reply).unwrap();
}
