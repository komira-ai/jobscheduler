//! What a service certificate does to the hosts the server holds: the deny list, a renewed
//! certificate on a running session, and a host bound to a join token that moves onto its
//! certificate.

use std::time::Duration;

use js_registry::Credential;
use js_types::HostId;
use serde_json::json;

use crate::agents_support::{JOIN, hello_body};
use crate::job_hosts::cpu_hello;
use crate::jobs_support::{JobBody, commands, one_start, running};
use crate::service_certs_support::{Client, Options, Rig, Spec, beat_of};
use crate::support::{Kept, LogCapture, TOKEN, kept_bindings};
use crate::tls::{connector, tls_request};

/// The code of a refusal.
fn code(reply: &crate::support::Reply) -> String {
    reply.json()["error"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

/// A host admitted with a one-time token that it swapped for a host credential: the
/// credential and the welcome of its hello.
async fn enrolled(client: &Client, host: &str) -> (String, crate::support::Reply) {
    let mint = json!({ "provider": "lambdalabs", "instance_id": "i-1" });
    let minted = client
        .send("POST", "/v1/agent/join-tokens", Some(TOKEN), Some(&mint))
        .await
        .unwrap();
    assert_eq!(minted.status, 201, "{}", minted.body);
    let token = minted.json()["token"].as_str().unwrap().to_owned();
    let mut hello = hello_body(host);
    hello["instance"] = json!({ "provider": "lambdalabs", "instance_id": "i-1" });
    hello["accepts_host_credential"] = json!(true);
    let welcome = client.hello(Some(&token), &hello).await.unwrap();
    assert_eq!(welcome.status, 200, "{}", welcome.body);
    let credential = welcome.json()["host_credential"]
        .as_str()
        .unwrap()
        .to_owned();
    (credential, welcome)
}

/// Asserts that a host on the deny list is refused 403 `host-denied` on every agent route, for
/// a certificate host (hello, heartbeat, own-host read), a host of the pool's join token (hello,
/// heartbeat) and a host with a host credential (own-host read, heartbeat), once a tick has read
/// the file and not before; that a host that is not listed is served as before; and that when
/// the lines are removed and a tick has run the hosts are served again on the sessions they
/// had.
#[tokio::test]
async fn a_denied_host_is_refused_on_every_route_until_it_is_readmitted() {
    let rig = Rig::start("service_cert_deny", Options::FILES).await;
    let cert = rig.client(Some(&rig.ca.issue(&Spec::agent("farm-1"))));
    let plain = rig.client(None);
    let welcome = cert.hello(None, &hello_body("farm-1")).await.unwrap();
    let cert_beat = beat_of(&welcome, "farm-1");
    let welcome = plain
        .hello(Some(JOIN), &hello_body("farm-2"))
        .await
        .unwrap();
    let token_beat = beat_of(&welcome, "farm-2");
    let (credential, welcome) = enrolled(&plain, "farm-3").await;
    let enrolled_beat = beat_of(&welcome, "farm-3");

    rig.deny("# retired\n\nfarm-1\nfarm-2\n  farm-3\n");
    let before = cert.beat(None, &cert_beat).await.unwrap();
    assert_eq!(
        before.status, 200,
        "the file is read at the tick: {}",
        before.body
    );
    rig.h.ticks.step().await;

    let denied = json!({ "error": "host-denied" });
    let refused = [
        cert.hello(None, &hello_body("farm-1")).await.unwrap(),
        cert.beat(None, &cert_beat).await.unwrap(),
        cert.host(None, "farm-1").await.unwrap(),
        plain
            .hello(Some(JOIN), &hello_body("farm-2"))
            .await
            .unwrap(),
        plain.beat(Some(JOIN), &token_beat).await.unwrap(),
        plain.host(Some(&credential), "farm-3").await.unwrap(),
        plain.beat(Some(&credential), &enrolled_beat).await.unwrap(),
    ];
    for reply in refused {
        assert_eq!(reply.status, 403, "{}", reply.body);
        assert_eq!(reply.json(), denied);
    }
    let other = plain
        .hello(Some(JOIN), &hello_body("farm-4"))
        .await
        .unwrap();
    assert_eq!(other.status, 200, "{}", other.body);

    rig.deny("farm-2\n");
    rig.h.ticks.step().await;
    let back = cert.beat(None, &cert_beat).await.unwrap();
    assert_eq!(back.status, 200, "{}", back.body);
    let own = cert.host(None, "farm-1").await.unwrap();
    assert_eq!(own.status, 200, "{}", own.body);
    let read = plain.host(Some(&credential), "farm-3").await.unwrap();
    assert_eq!(read.status, 200, "{}", read.body);
    let still = plain.beat(Some(JOIN), &token_beat).await.unwrap();
    assert_eq!(code(&still), "host-denied");
}

/// Asserts that a one-time join token is not used up by a hello of a host on the deny list: the
/// hello is refused `host-denied` and, once the host is taken off the list and a tick has read
/// the file, the same token is admitted.
#[tokio::test]
async fn a_denied_hello_does_not_use_up_the_hosts_one_time_token() {
    let rig = Rig::start("service_cert_deny_token", Options::FILES).await;
    let plain = rig.client(None);
    let instance = json!({ "provider": "lambdalabs", "instance_id": "i-1" });
    let minted = plain
        .send(
            "POST",
            "/v1/agent/join-tokens",
            Some(TOKEN),
            Some(&instance),
        )
        .await
        .unwrap();
    assert_eq!(minted.status, 201, "{}", minted.body);
    let token = minted.json()["token"].as_str().unwrap().to_owned();
    let mut hello = hello_body("farm-1");
    hello["instance"] = instance;

    rig.deny("farm-1\n");
    rig.h.ticks.step().await;
    let denied = plain.hello(Some(&token), &hello).await.unwrap();
    assert_eq!(
        (denied.status, code(&denied)),
        (403, "host-denied".to_owned())
    );

    rig.deny("");
    rig.h.ticks.step().await;
    let admitted = plain.hello(Some(&token), &hello).await.unwrap();
    assert_eq!(admitted.status, 200, "{}", admitted.body);
}

/// Asserts that a certificate holder asking about another host that is on the deny list is
/// answered `certificate-host-mismatch` on every route, never `host-denied`, so that it learns
/// nothing of the deny status of hosts that are not its own.
#[tokio::test]
async fn a_certificate_asking_about_a_denied_other_host_learns_only_the_mismatch() {
    let rig = Rig::start("service_cert_deny_other", Options::FILES).await;
    let cert = rig.client(Some(&rig.ca.issue(&Spec::agent("farm-1"))));
    let welcome = cert.hello(None, &hello_body("farm-1")).await.unwrap();
    let mut beat = beat_of(&welcome, "farm-1");
    beat["host_id"] = json!("farm-2");
    rig.deny("farm-2\n");
    rig.h.ticks.step().await;

    let replies = [
        cert.hello(None, &hello_body("farm-2")).await.unwrap(),
        cert.beat(None, &beat).await.unwrap(),
        cert.host(None, "farm-2").await.unwrap(),
    ];
    for reply in replies {
        assert_eq!(
            (reply.status, code(&reply)),
            (403, "certificate-host-mismatch".to_owned()),
            "{}",
            reply.body
        );
    }
}

/// Asserts that a host on the deny list cannot cordon or uncordon itself on its own-host route,
/// with its certificate or with its host credential: both are refused `host-denied`.
#[tokio::test]
async fn a_denied_host_cannot_cordon_or_uncordon_itself() {
    let rig = Rig::start("service_cert_deny_cordon", Options::FILES).await;
    let cert = rig.client(Some(&rig.ca.issue(&Spec::agent("farm-1"))));
    let welcome = cert.hello(None, &hello_body("farm-1")).await.unwrap();
    assert_eq!(welcome.status, 200, "{}", welcome.body);
    let plain = rig.client(None);
    let (credential, _) = enrolled(&plain, "farm-2").await;
    rig.deny("farm-1\nfarm-2\n");
    rig.h.ticks.step().await;

    let body = json!({});
    for verb in ["cordon", "uncordon"] {
        let route = |host: &str| format!("/v1/hosts/{host}:{verb}");
        let by_certificate = cert
            .send("POST", &route("farm-1"), None, Some(&body))
            .await
            .unwrap();
        let by_credential = plain
            .send("POST", &route("farm-2"), Some(&credential), Some(&body))
            .await
            .unwrap();
        for reply in [by_certificate, by_credential] {
            assert_eq!(
                (reply.status, code(&reply)),
                (403, "host-denied".to_owned()),
                "{verb}: {}",
                reply.body
            );
        }
    }
}

/// Asserts that a certificate host's hello limit is not reset by a tick: with a burst of two,
/// the third hello of one certificate host is held `rate-limited` (429), and is still held after
/// a tick has run, while a tick that has run does not give it a new burst.
#[tokio::test]
async fn a_tick_does_not_reset_the_hello_limit_of_a_certificate_host() {
    let options = Options {
        burst: Some("2"),
        ..Options::FILES
    };
    let rig = Rig::start("service_cert_rate", options).await;
    let cert = rig.client(Some(&rig.ca.issue(&Spec::agent("farm-1"))));
    let first = cert.hello(None, &hello_body("farm-1")).await.unwrap();
    assert_eq!(first.status, 200, "{}", first.body);
    let live = cert.hello(None, &hello_body("farm-1")).await.unwrap();
    assert_eq!((live.status, code(&live)), (409, "host-live".to_owned()));
    let held = cert.hello(None, &hello_body("farm-1")).await.unwrap();
    assert_eq!((held.status, code(&held)), (429, "rate-limited".to_owned()));

    rig.h.ticks.step().await;
    let still = cert.hello(None, &hello_body("farm-1")).await.unwrap();
    assert_eq!(
        (still.status, code(&still)),
        (429, "rate-limited".to_owned())
    );
}

/// Asserts that a tick that cannot read the deny file keeps the hosts denied before it and
/// logs why, and that a tick that can read it again follows it: the host stays refused while
/// the file is gone, and is served once the file is back without its line.
#[tokio::test]
async fn an_unreadable_deny_file_keeps_the_hosts_denied() {
    let logs = LogCapture::start();
    let rig = Rig::start("service_cert_deny_gone", Options::FILES).await;
    let plain = rig.client(None);
    let welcome = plain
        .hello(Some(JOIN), &hello_body("farm-2"))
        .await
        .unwrap();
    let beat = beat_of(&welcome, "farm-2");
    rig.deny("farm-2\n");
    rig.h.ticks.step().await;
    std::fs::remove_file(&rig.deny_file).unwrap();
    rig.h.ticks.step().await;

    let refused = plain.beat(Some(JOIN), &beat).await.unwrap();
    assert_eq!(code(&refused), "host-denied");
    assert!(
        logs.text()
            .contains("cannot read the agent deny file again"),
        "{}",
        logs.text()
    );
    rig.deny("");
    rig.h.ticks.step().await;
    let served = plain.beat(Some(JOIN), &beat).await.unwrap();
    assert_eq!(served.status, 200, "{}", served.body);
}

/// Asserts that a certificate that was renewed (a new key and serial, the same host) carries on
/// the session of the one before it: a heartbeat with the renewed certificate on the old
/// session is admitted at the same epoch, answers no new start for the job that is running and no
/// stop for it, and the job still reads running; no hello was needed.
#[tokio::test]
async fn a_renewed_certificate_carries_on_the_same_session_and_job() {
    let options = Options {
        jobs: true,
        ..Options::FILES
    };
    let rig = Rig::start("service_cert_renewal", options).await;
    let first = rig.client(Some(&rig.ca.issue(&Spec::agent("farm-1").serial(1))));
    let welcome = first.hello(None, &cpu_hello("farm-1")).await.unwrap();
    let beat = beat_of(&welcome, "farm-1");
    let job = rig.h.running.job_addr().unwrap();
    let trusted = connector(&rig.server_cert);
    let body = JobBody::new("renewed-1")
        .needs(6000, 1 << 30)
        .wall(900)
        .json();
    let body = body.to_string().into_bytes();
    let submitted = tls_request(job, &trusted, "POST", "/v1/jobs", Some(TOKEN), Some(&body)).await;
    assert_eq!(submitted.status, 201, "{}", submitted.body);

    let started = first.beat(None, &beat).await.unwrap();
    assert_eq!(started.status, 200, "{}", started.body);
    assert_eq!(one_start(&started.json()).0, "renewed-1");
    let mut listing = beat.clone();
    listing["jobs"] = json!([running("renewed-1")]);
    let held = first.beat(None, &listing).await.unwrap().json();
    assert_eq!(commands(&held), (vec![], vec![]), "{held}");

    let renewed = rig.client(Some(&rig.ca.issue(&Spec::agent("farm-1").serial(2))));
    let carried = renewed.beat(None, &listing).await.unwrap();
    assert_eq!(carried.status, 200, "{}", carried.body);
    assert_eq!(
        commands(&carried.json()),
        (vec![], vec![]),
        "{}",
        carried.body
    );
    let own = renewed.host(None, "farm-1").await.unwrap().json();
    assert_eq!(
        (own["epoch"].clone(), own["state"].clone()),
        (json!(1), json!("live"))
    );
    let read = tls_request(
        job,
        &trusted,
        "GET",
        "/v1/jobs/renewed-1",
        Some(TOKEN),
        None,
    )
    .await;
    assert_eq!(
        read.json()["job"]["state"],
        "JOB_STATE_RUNNING",
        "{}",
        read.body
    );
}

/// The digest of the pool's join token, as the bindings hold it.
fn token_digest() -> String {
    Credential::of_token(JOIN).to_hex()
}

/// The digest of the service certificate credential of `host`.
fn certificate_digest(host: &str) -> String {
    Credential::of_service_certificate(&HostId::new(host).unwrap()).to_hex()
}

/// A server keeping its bindings as `kept` says, a host `farm-1` live on the pool's join token,
/// and a client presenting that host's service certificate.
async fn kept_token_bound(kept: Kept) -> (Rig, Client) {
    let options = Options {
        kept,
        ..Options::FILES
    };
    let rig = Rig::start("service_cert_rebind", options).await;
    let plain = rig.client(None);
    let welcome = plain
        .hello(Some(JOIN), &hello_body("farm-1"))
        .await
        .unwrap();
    assert_eq!(welcome.status, 200, "{}", welcome.body);
    let cert = rig.client(Some(&rig.ca.issue(&Spec::agent("farm-1"))));
    (rig, cert)
}

/// Asserts, with the bindings in files and in the store, that a host bound to the pool's join
/// token is refused its certificate's hello `host-live` while it is live, with the binding
/// still the token's; that once it is lost the certificate's hello is admitted at the next epoch
/// and the binding that is kept holds the certificate's credential and no longer the token's;
/// that the pool's token is then refused `host-bound` on a hello and on the old session's
/// heartbeat; and that the certificate carries on beating.
#[tokio::test]
async fn a_lost_token_host_is_rebound_to_its_certificate_by_its_first_hello() {
    for kept in Kept::BOTH {
        let (rig, cert) = kept_token_bound(kept).await;
        let plain = rig.client(None);
        let live = cert.hello(None, &hello_body("farm-1")).await.unwrap();
        assert_eq!((live.status, code(&live)), (409, "host-live".to_owned()));
        let held = kept_bindings(&rig.h);
        assert!(held.contains(&token_digest()), "{kept:?}: {held}");

        rig.h.clock.advance(Duration::from_secs(60));
        let welcome = cert.hello(None, &hello_body("farm-1")).await.unwrap();
        assert_eq!(welcome.status, 200, "{kept:?}: {}", welcome.body);
        assert_eq!(welcome.json()["epoch"], 2);
        let kept_now = kept_bindings(&rig.h);
        assert!(
            kept_now.contains(&certificate_digest("farm-1")),
            "{kept:?}: {kept_now}"
        );
        assert!(!kept_now.contains(&token_digest()), "{kept:?}: {kept_now}");

        let taken = plain
            .hello(Some(JOIN), &hello_body("farm-1"))
            .await
            .unwrap();
        assert_eq!((taken.status, code(&taken)), (403, "host-bound".to_owned()));
        let beat = cert.beat(None, &beat_of(&welcome, "farm-1")).await.unwrap();
        assert_eq!(beat.status, 200, "{kept:?}: {}", beat.body);
    }
}

/// Asserts that a certificate host is never taken by a join token, live or lost: the pool's
/// token is refused `host-bound` at its hello, and its heartbeat on the certificate's session
/// is refused too.
#[tokio::test]
async fn a_join_token_never_takes_a_certificate_host() {
    let rig = Rig::start("service_cert_never_token", Options::FILES).await;
    let cert = rig.client(Some(&rig.ca.issue(&Spec::agent("farm-1"))));
    let plain = rig.client(None);
    let welcome = cert.hello(None, &hello_body("farm-1")).await.unwrap();
    let live = plain
        .hello(Some(JOIN), &hello_body("farm-1"))
        .await
        .unwrap();
    assert_eq!(code(&live), "host-bound");
    rig.h.clock.advance(Duration::from_secs(60));
    let lost = plain
        .hello(Some(JOIN), &hello_body("farm-1"))
        .await
        .unwrap();
    assert_eq!(code(&lost), "host-bound");
    let beat = plain
        .beat(Some(JOIN), &beat_of(&welcome, "farm-1"))
        .await
        .unwrap();
    assert_eq!(code(&beat), "host-bound");
}

/// Asserts, with the bindings in files and in the store, that a host that moved onto its
/// certificate keeps what it had across the move and a restart: a host enrolled with a one-time
/// token cordons itself, is lost, says hello with its certificate and is bound to it with its
/// cordon in force; after the server restarts, the pool's token is still refused `host-bound`
/// for it and the certificate's hello is admitted, because the binding that was kept is the
/// certificate's.
#[tokio::test]
async fn the_move_onto_a_certificate_keeps_the_cordon_across_a_restart() {
    for kept in Kept::BOTH {
        let options = Options {
            kept,
            ..Options::FILES
        };
        let rig = Rig::start("service_cert_rebind_restart", options).await;
        let plain = rig.client(None);
        let (credential, _) = enrolled(&plain, "farm-1").await;
        let cordon = plain
            .send(
                "POST",
                "/v1/hosts/farm-1:cordon",
                Some(&credential),
                Some(&json!({})),
            )
            .await
            .unwrap();
        assert_eq!(cordon.status, 200, "{}", cordon.body);
        rig.h.clock.advance(Duration::from_secs(60));
        let cert = rig.client(Some(&rig.ca.issue(&Spec::agent("farm-1"))));
        let welcome = cert.hello(None, &hello_body("farm-1")).await.unwrap();
        assert_eq!(welcome.status, 200, "{kept:?}: {}", welcome.body);
        let own = cert.host(None, "farm-1").await.unwrap().json();
        assert!(own.get("cordon").is_some(), "{kept:?}: {own}");
        let held = kept_bindings(&rig.h);
        assert!(
            held.contains(&certificate_digest("farm-1")),
            "{kept:?}: {held}"
        );
        assert!(held.contains("cordon_until_ms"), "{kept:?}: {held}");

        let rig = rig.restarted().await;
        let plain = rig.client(None);
        let taken = plain
            .hello(Some(JOIN), &hello_body("farm-1"))
            .await
            .unwrap();
        assert_eq!(
            (taken.status, code(&taken)),
            (403, "host-bound".to_owned()),
            "{kept:?}"
        );
        let cert = rig.client(Some(&rig.ca.issue(&Spec::agent("farm-1"))));
        let again = cert.hello(None, &hello_body("farm-1")).await.unwrap();
        assert_eq!(again.status, 200, "{kept:?}: {}", again.body);
    }
}
