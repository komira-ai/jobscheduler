//! What a host certificate does to the hosts the server holds: a renewed
//! certificate on a running session, and the cordon of a certificate host across a restart.

use js_registry::Credential;
use js_types::HostId;
use serde_json::json;

use crate::agents_support::hello_body;
use crate::host_certs_support::{Options, Rig, Spec, beat_of};
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

/// The digest of the host certificate credential of `host`.
fn certificate_digest(host: &str) -> String {
    Credential::of_host_certificate(&HostId::new(host).unwrap()).to_hex()
}

/// Asserts, with the bindings in files and in the store, that a certificate host keeps its
/// cordon across a restart: the host says hello, cordons itself, and its binding holds the
/// certificate's credential and the cordon; after the server restarts, its hello with the
/// certificate is admitted and its own read still shows the cordon.
#[tokio::test]
async fn a_certificate_host_keeps_its_cordon_across_a_restart() {
    for kept in Kept::BOTH {
        let options = Options {
            kept,
            ..Options::FILES
        };
        let rig = Rig::start("host_cert_cordon_restart", options).await;
        let cert = rig.client(Some(&rig.ca.issue(&Spec::host("farm-1"))));
        let welcome = cert.hello(None, &hello_body("farm-1")).await.unwrap();
        assert_eq!(welcome.status, 200, "{kept:?}: {}", welcome.body);
        let cordon = cert
            .send("POST", "/v1/hosts/farm-1:cordon", None, Some(&json!({})))
            .await
            .unwrap();
        assert_eq!(cordon.status, 200, "{}", cordon.body);
        let held = kept_bindings(&rig.h);
        assert!(
            held.contains(&certificate_digest("farm-1")),
            "{kept:?}: {held}"
        );
        assert!(held.contains("cordon_until_ms"), "{kept:?}: {held}");

        let rig = rig.restarted().await;
        let cert = rig.client(Some(&rig.ca.issue(&Spec::host("farm-1"))));
        let again = cert.hello(None, &hello_body("farm-1")).await.unwrap();
        assert_eq!(again.status, 200, "{kept:?}: {}", again.body);
        let own = cert.host(None, "farm-1").await.unwrap().json();
        assert!(own.get("cordon").is_some(), "{kept:?}: {own}");
    }
}
