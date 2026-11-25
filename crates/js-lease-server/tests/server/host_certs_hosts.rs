//! What a host certificate does to the hosts the server holds: a renewed
//! certificate on a running session, and a host bound to a join token that moves onto its
//! certificate.

use std::time::Duration;

use js_registry::Credential;
use js_types::HostId;
use serde_json::json;

use crate::agents_support::{JOIN, hello_body};
use crate::host_certs_support::{Client, Options, Rig, Spec, beat_of};
use crate::job_hosts::cpu_hello;
use crate::jobs_support::{JobBody, commands, one_start, running};
use crate::support::{Kept, TOKEN, kept_bindings};
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

/// Asserts that a certificate host's hello limit is not reset by a tick: with a burst of two,
/// the third hello of one certificate host is held `rate-limited` (429), and is still held after
/// a tick has run, while a tick that has run does not give it a new burst.
#[tokio::test]
async fn a_tick_does_not_reset_the_hello_limit_of_a_certificate_host() {
    let options = Options {
        burst: Some("2"),
        ..Options::FILES
    };
    let rig = Rig::start("host_cert_rate", options).await;
    let cert = rig.client(Some(&rig.ca.issue(&Spec::host("farm-1"))));
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
    let rig = Rig::start("host_cert_renewal", options).await;
    let first = rig.client(Some(&rig.ca.issue(&Spec::host("farm-1").serial(1))));
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
    // The submit answers the job queued; the placement poller places it on its own task, and
    // the start rides on the first beat after that placement.
    let mut reads = 0;
    while tls_request(
        job,
        &trusted,
        "GET",
        "/v1/jobs/renewed-1",
        Some(TOKEN),
        None,
    )
    .await
    .json()["job"]["state"]
        == "JOB_STATE_QUEUED"
    {
        reads += 1;
        assert!(
            reads < 10_000,
            "renewed-1 is still queued after {reads} reads"
        );
        tokio::task::yield_now().await;
    }

    let started = first.beat(None, &beat).await.unwrap();
    assert_eq!(started.status, 200, "{}", started.body);
    assert_eq!(one_start(&started.json()).0, "renewed-1");
    let mut listing = beat.clone();
    listing["jobs"] = json!([running("renewed-1")]);
    let held = first.beat(None, &listing).await.unwrap().json();
    assert_eq!(commands(&held), (vec![], vec![]), "{held}");

    let renewed = rig.client(Some(&rig.ca.issue(&Spec::host("farm-1").serial(2))));
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

/// The digest of the host certificate credential of `host`.
fn certificate_digest(host: &str) -> String {
    Credential::of_host_certificate(&HostId::new(host).unwrap()).to_hex()
}

/// A server keeping its bindings as `kept` says, a host `farm-1` live on the pool's join token,
/// and a client presenting that host's host certificate.
async fn kept_token_bound(kept: Kept) -> (Rig, Client) {
    let options = Options {
        kept,
        ..Options::FILES
    };
    let rig = Rig::start("host_cert_rebind", options).await;
    let plain = rig.client(None);
    let welcome = plain
        .hello(Some(JOIN), &hello_body("farm-1"))
        .await
        .unwrap();
    assert_eq!(welcome.status, 200, "{}", welcome.body);
    let cert = rig.client(Some(&rig.ca.issue(&Spec::host("farm-1"))));
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
    let rig = Rig::start("host_cert_never_token", Options::FILES).await;
    let cert = rig.client(Some(&rig.ca.issue(&Spec::host("farm-1"))));
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
        let rig = Rig::start("host_cert_rebind_restart", options).await;
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
        let cert = rig.client(Some(&rig.ca.issue(&Spec::host("farm-1"))));
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
        let cert = rig.client(Some(&rig.ca.issue(&Spec::host("farm-1"))));
        let again = cert.hello(None, &hello_body("farm-1")).await.unwrap();
        assert_eq!(again.status, 200, "{kept:?}: {}", again.body);
    }
}
