//! The host credential that replaces a one-time join token at the first hello: issued only
//! to a hello that asks for it, the token dead afterwards, the credential carrying the
//! host's later hellos and heartbeats, revoked with the instance, and never shown twice.

use std::fs;
use std::time::Duration;

use serde_json::{Value, json};

use crate::agents_support::{
    JOIN, Session, hello_body, hosts, listed, post, restarted, session, start_agents,
};
use crate::joins::{MINT, beat_with, code, hello, minted};
use crate::support::{Harness, LogCapture, Reply};

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// The hello of `host`, on `instance`, that says it keeps a host credential.
fn asking_body(host: &str, instance: &str) -> Value {
    let mut body = hello_body(host);
    body["instance"] = json!({ "provider": "lambdalabs", "instance_id": instance });
    body["accepts_host_credential"] = json!(true);
    body
}

/// A hello that asks for a credential, with `secret` as the bearer.
async fn asking(h: &Harness, secret: &str, host: &str, instance: &str) -> Reply {
    post(
        h,
        "/v1/agent/hello",
        Some(secret),
        &asking_body(host, instance),
    )
    .await
}

/// The credential in a welcome.
fn credential(welcome: &Reply) -> String {
    let text = welcome.json()["host_credential"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(text.len(), 64, "{text}");
    assert!(text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')));
    text
}

fn joins_text(h: &Harness) -> String {
    let mut joins = h.scratch.state().into_os_string();
    joins.push(".joins");
    fs::read_to_string(joins).unwrap()
}

/// Asserts the whole life of a credential: a hello that asks and registers a host with an
/// unused token gets a 64-digit credential; from then on the token gets the 401 of an
/// unknown token on a heartbeat, a hello of the same host and a hello of another; the
/// credential carries heartbeats, and the host's next hello (after its session lapsed) with
/// it is admitted without a second credential; the credential registers no other host id
/// and heartbeats for none. It catches a token that outlives its swap, a credential that
/// carries another host, and a second credential for a host that has one.
#[tokio::test]
async fn the_first_hello_that_asks_swaps_the_token_for_a_credential() {
    let h = start_agents("cred_swap", false).await;
    let token = minted(&h, "i-1").await;
    let welcome = asking(&h, &token, "burst-1", "i-1").await;
    let secret = credential(&welcome);
    assert_ne!(secret, token);
    let held = session(&welcome, "burst-1");
    for refused in [
        beat_with(&h, &token, &held).await,
        asking(&h, &token, "burst-1", "i-1").await,
        asking(&h, &token, "burst-2", "i-1").await,
    ] {
        assert_eq!(refused.status, 401, "{}", refused.body);
        assert!(refused.body.is_empty(), "{}", refused.body);
    }
    assert_eq!(beat_with(&h, &secret, &held).await.status, 200);
    let stolen = Session {
        host: "burst-2".into(),
        ..held.clone()
    };
    let beat = beat_with(&h, &secret, &stolen).await;
    assert_eq!(
        (beat.status, code(&beat)),
        (403, "join-token-not-admitted".into())
    );
    let other = asking(&h, &secret, "burst-2", "i-1").await;
    assert_eq!(
        (other.status, code(&other)),
        (403, "join-token-used".into())
    );

    h.clock.advance(secs(60));
    let again = asking(&h, &secret, "burst-1", "i-1").await;
    assert_eq!(again.status, 200, "{}", again.body);
    assert!(
        again.json().get("host_credential").is_none(),
        "{}",
        again.body
    );
    let renewed = session(&again, "burst-1");
    assert_eq!(beat_with(&h, &secret, &renewed).await.status, 200);
    assert_eq!(listed(&hosts(&h).await), ["burst-1"]);
}

/// Asserts that a hello that does not ask for a credential (an agent from before the field)
/// is welcomed without one and keeps its token for heartbeats and for a hello after its
/// session lapsed, and that a pool join token is never swapped, also for a hello that
/// asks: the hand-joined path is as it was. It catches a server that retires the token of
/// an agent that cannot take a credential, and one that issues credentials on the pool's
/// shared token.
#[tokio::test]
async fn an_older_agent_and_a_pool_token_keep_the_token_path() {
    let h = start_agents("cred_older", false).await;
    let token = minted(&h, "i-1").await;
    let welcome = hello(&h, &token, "burst-1", "i-1").await;
    assert!(
        welcome.json().get("host_credential").is_none(),
        "{}",
        welcome.body
    );
    let held = session(&welcome, "burst-1");
    assert_eq!(beat_with(&h, &token, &held).await.status, 200);
    h.clock.advance(secs(60));
    assert_eq!(hello(&h, &token, "burst-1", "i-1").await.status, 200);

    let pooled = post(&h, "/v1/agent/hello", Some(JOIN), &{
        let mut body = hello_body("burst-2");
        body["accepts_host_credential"] = json!(true);
        body
    })
    .await;
    assert!(
        pooled.json().get("host_credential").is_none(),
        "{}",
        pooled.body
    );
    let second = session(&pooled, "burst-2");
    assert_eq!(beat_with(&h, JOIN, &second).await.status, 200);
}

/// Asserts that the credential outlives a restart of the server: the file still holds its
/// digest and the host, so a hello with it after the restart is admitted without a second
/// credential, its heartbeats serve, and the retired token is still a 401. It catches a
/// credential kept in memory only, and a retired token that a restart gives back.
#[tokio::test]
async fn the_credential_outlives_a_restart() {
    let h = start_agents("cred_restart", false).await;
    let token = minted(&h, "i-1").await;
    let secret = credential(&asking(&h, &token, "burst-1", "i-1").await);
    let h = restarted(h, false).await;
    assert_eq!(hello(&h, &token, "burst-1", "i-1").await.status, 401);
    let welcome = asking(&h, &secret, "burst-1", "i-1").await;
    assert_eq!(welcome.status, 200, "{}", welcome.body);
    assert!(welcome.json().get("host_credential").is_none());
    let held = session(&welcome, "burst-1");
    assert_eq!(beat_with(&h, &secret, &held).await.status, 200);
}

/// Asserts that the welcome that carries the credential can be lost: the host holds
/// nothing, so its token (retired when the credential was made) gets the 401 of an unknown
/// token on a retry from the same host id and on one from another, and the release the
/// scaler makes when the boot time runs out (the revoke of the instance, a 204) leaves no
/// row in the file, so a new launch of the instance registers a new host with a new token.
/// It catches a token that stays good after its credential was issued, which would let a
/// host that never heard its welcome register a second time.
#[tokio::test]
async fn a_lost_welcome_leaves_no_way_in_until_the_instance_is_released() {
    let h = start_agents("cred_lost", false).await;
    let token = minted(&h, "i-1").await;
    let lost = asking(&h, &token, "burst-1", "i-1").await;
    assert_eq!(lost.status, 200, "{}", lost.body);
    drop(lost);
    for host in ["burst-1", "burst-2"] {
        let retry = asking(&h, &token, host, "i-1").await;
        assert_eq!(retry.status, 401, "{}", retry.body);
    }
    assert_eq!(joins_text(&h).matches("[[join]]").count(), 1);
    let body = json!({ "provider": "lambdalabs", "instance_id": "i-1" });
    let released = h.call("DELETE", MINT, Some(&body)).await;
    assert_eq!((released.status, released.body.as_str()), (204, ""));
    assert_eq!(joins_text(&h).matches("[[join]]").count(), 0);
    let fresh = minted(&h, "i-1").await;
    let welcome = asking(&h, &fresh, "burst-3", "i-1").await;
    assert_eq!(welcome.status, 200, "{}", welcome.body);
    credential(&welcome);
}

/// Asserts that the revoke of the instance (the terminate path) ends the credential: its
/// heartbeat and its hello get the 401 of an unknown secret, also after a restart, and
/// the file holds no row for the host. It catches a credential that survives the release
/// of its instance.
#[tokio::test]
async fn a_revoke_ends_the_credential() {
    let h = start_agents("cred_revoke", false).await;
    let token = minted(&h, "i-1").await;
    let welcome = asking(&h, &token, "burst-1", "i-1").await;
    let secret = credential(&welcome);
    let held = session(&welcome, "burst-1");
    assert_eq!(beat_with(&h, &secret, &held).await.status, 200);
    let body = json!({ "provider": "lambdalabs", "instance_id": "i-1" });
    assert_eq!(h.call("DELETE", MINT, Some(&body)).await.status, 204);
    for refused in [
        beat_with(&h, &secret, &held).await,
        asking(&h, &secret, "burst-1", "i-1").await,
    ] {
        assert_eq!(refused.status, 401, "{}", refused.body);
        assert!(refused.body.is_empty());
    }
    assert!(!joins_text(&h).contains("burst-1"));
    let h = restarted(h, false).await;
    assert_eq!(asking(&h, &secret, "burst-1", "i-1").await.status, 401);
}

/// Asserts that the credential carries the silence bound of a token: a day without a
/// heartbeat and it is refused as expired (403 `join-token-expired`), by heartbeat and by
/// hello. (That a heartbeat inside the day moves the bound is proved at the store.) It
/// catches a credential that never ends.
#[tokio::test]
async fn the_credential_ends_after_a_day_of_silence() {
    let h = start_agents("cred_silence", false).await;
    let token = minted(&h, "i-1").await;
    let welcome = asking(&h, &token, "burst-1", "i-1").await;
    let secret = credential(&welcome);
    let held = session(&welcome, "burst-1");
    h.clock.advance(secs(24 * 3600));
    for refused in [
        beat_with(&h, &secret, &held).await,
        asking(&h, &secret, "burst-1", "i-1").await,
    ] {
        assert_eq!(
            (refused.status, code(&refused)),
            (403, "join-token-expired".into())
        );
    }
}

/// Asserts that the credential is shown once and kept nowhere: it is in the one welcome
/// that carried it, and in no other answer (the refusals, the heartbeat reply, the hosts
/// listing, the second welcome), no log line, and no file of the server, which holds its
/// digest only. It catches a credential in a log, an error or the file.
#[tokio::test]
async fn the_credential_is_shown_once_and_stored_never() {
    let logs = LogCapture::start();
    let h = start_agents("cred_secret", false).await;
    let token = minted(&h, "i-1").await;
    let welcome = asking(&h, &token, "burst-1", "i-1").await;
    let secret = credential(&welcome);
    let held = session(&welcome, "burst-1");
    let mut answers = vec![
        beat_with(&h, &secret, &held).await,
        beat_with(&h, &token, &held).await,
        asking(&h, &token, "burst-2", "i-1").await,
        asking(&h, &format!("{secret}x"), "burst-1", "i-1").await,
        asking(&h, &secret, "burst-2", "i-1").await,
    ];
    h.clock.advance(secs(60));
    answers.push(asking(&h, &secret, "burst-1", "i-1").await);
    answers.push(h.call("GET", "/v1/hosts", None).await);
    h.ticks.step().await;
    for answer in &answers {
        assert!(!answer.body.contains(&secret), "{}", answer.body);
    }
    let text = logs.text();
    assert!(text.contains("agent admitted"), "{text}");
    assert!(text.contains("credential_issued=true"), "{text}");
    assert!(!text.contains(&secret), "{text}");
    assert!(!text.contains(&token), "{text}");
    for entry in fs::read_dir(&h.scratch.dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_file() {
            let held = String::from_utf8_lossy(&fs::read(&path).unwrap()).into_owned();
            assert!(!held.contains(&secret), "{}", path.display());
            assert!(!held.contains(&token), "{}", path.display());
        }
    }
}
