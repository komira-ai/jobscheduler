//! Host certificates over a real TLS listener on 127.0.0.1: which certificates
//! the handshake accepts, what a certificate is the credential for, and that token agents and
//! lease clients are served on the same listener with the CA configured.

use rcgen::ExtendedKeyUsagePurpose as Usage;
use serde_json::{Value, json};

use crate::agents_support::{JOIN, hello_body};
use crate::host_certs_support::{AGENT_ROLE_URI, Ca, Options, Rig, Spec, beat_of};
use crate::support::TOKEN;

/// The code of a refusal.
fn code(reply: &crate::support::Reply) -> String {
    reply.json()["error"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

/// Asserts that the host certificate of a host, from the CA the server was given,
/// is the whole credential: with no bearer it opens a session by hello, beats it, reads the
/// host's own state and cordons it, and the welcome carries no host credential. A bearer that
/// is not a token of the pool beside the certificate changes nothing: the certificate is the
/// credential and the bearer is not consulted.
#[tokio::test]
async fn a_host_certificate_is_the_whole_credential_of_its_host() {
    let rig = Rig::start("host_cert_whole", Options::FILES).await;
    let id = rig.ca.issue(&Spec::host("farm-1"));
    let client = rig.client(Some(&id));

    let hello = client.hello(None, &hello_body("farm-1")).await.unwrap();
    assert_eq!(hello.status, 200, "{}", hello.body);
    assert!(
        hello.json().get("host_credential").is_none(),
        "{}",
        hello.body
    );
    let beat = beat_of(&hello, "farm-1");
    let beaten = client.beat(None, &beat).await.unwrap();
    assert_eq!(beaten.status, 200, "{}", beaten.body);
    let own = client.host(None, "farm-1").await.unwrap();
    assert_eq!(own.status, 200, "{}", own.body);
    assert_eq!(own.json()["state"], "live");
    assert_eq!(own.json()["epoch"], 1);
    let cordon = client
        .send("POST", "/v1/hosts/farm-1:cordon", None, Some(&json!({})))
        .await
        .unwrap();
    assert_eq!(cordon.status, 200, "{}", cordon.body);

    let ignored = client.beat(Some("not-a-token"), &beat).await.unwrap();
    assert_eq!(ignored.status, 200, "{}", ignored.body);
    let listed = rig.listing().await;
    assert!(listed.to_string().contains("farm-1"), "{listed}");
}

/// Asserts that a client that speaks TLS 1.2 only is admitted with its certificate as well, so
/// the verifier's TLS 1.2 signature check is the web PKI verifier's.
#[tokio::test]
async fn a_certificate_is_admitted_over_tls_1_2() {
    let rig = Rig::start("host_cert_tls12", Options::FILES).await;
    let id = rig.ca.issue(&Spec::host("farm-1"));
    let client = rig.client(Some(&id)).tls12();
    let hello = client.hello(None, &hello_body("farm-1")).await.unwrap();
    assert_eq!(hello.status, 200, "{}", hello.body);
}

/// Asserts that a host certificate is admitted whatever hostkeeper puts besides its one DNS
/// SAN: the host-role URI with the CN `host:<id>`, the agent-role URI, no URI at all, and no
/// CN. Each opens a session for the host its DNS SAN names, and none needs a service name.
#[tokio::test]
async fn a_host_certificate_is_admitted_whatever_uri_and_cn_it_carries() {
    let rig = Rig::start("host_cert_shapes", Options::FILES).await;
    let shapes = [
        ("as hostkeeper issues it", Spec::host("farm-1")),
        (
            "with the agent role",
            Spec {
                uris: vec![AGENT_ROLE_URI.to_owned()],
                ..Spec::host("farm-2")
            },
        ),
        (
            "with no uri",
            Spec {
                uris: vec![],
                ..Spec::host("farm-3")
            },
        ),
        (
            "with no cn",
            Spec {
                cn: None,
                ..Spec::host("farm-4")
            },
        ),
    ];
    for (what, spec) in shapes {
        let host = spec.dns[0].clone();
        let client = rig.client(Some(&rig.ca.issue(&spec)));
        let hello = client.hello(None, &hello_body(&host)).await.unwrap();
        assert_eq!(hello.status, 200, "{what}: {}", hello.body);
    }
}

/// Asserts that each certificate that is not a host certificate of the CA fails before any
/// request is answered: one of another CA, one that is also for servers, one with no extended
/// key usage, one that is a CA's, one with no DNS name (the host's id only in its CN, as
/// hostkeeper's certificates were before they carried the SAN), with two, with a name that is
/// no host id, and one that has expired. A control shows the same client with a good
/// certificate is admitted.
#[tokio::test]
async fn a_certificate_that_is_not_a_host_certificate_fails_the_handshake() {
    let rig = Rig::start("host_cert_refused", Options::FILES).await;
    let host = Spec::host("farm-1");
    let with = |change: &dyn Fn(&mut Spec)| {
        let mut spec = host.clone();
        change(&mut spec);
        rig.ca.issue(&spec)
    };
    let refused = [
        ("another CA", Ca::new().issue(&host)),
        (
            "server auth too",
            with(&|s| s.usages = vec![Usage::ClientAuth, Usage::ServerAuth]),
        ),
        ("no key usage", with(&|s| s.usages = vec![])),
        ("a CA certificate", with(&|s| s.ca = true)),
        ("no dns name, the id in the CN", with(&|s| s.dns = vec![])),
        (
            "two dns names",
            with(&|s| s.dns = vec!["farm-1".to_owned(), "farm-2".to_owned()]),
        ),
        (
            "a bad host id",
            with(&|s| s.dns = vec!["Not_A_Host".to_owned()]),
        ),
        ("expired", with(&|s| s.years = (2020, 2021))),
    ];
    for (what, id) in refused {
        let client = rig.client(Some(&id));
        let failed = client.hello(None, &hello_body("farm-1")).await;
        assert!(failed.is_err(), "{what}: {failed:?}");
    }
    let control = rig.client(Some(&rig.ca.issue(&host)));
    let admitted = control.hello(None, &hello_body("farm-1")).await.unwrap();
    assert_eq!(admitted.status, 200, "{}", admitted.body);
}

/// Asserts that a body or a path naming another host than the certificate's is refused 403
/// `certificate-host-mismatch` on every agent route: the hello, the heartbeat, the own-host
/// read and both own-host verbs, and that nothing of the other host was admitted.
#[tokio::test]
async fn a_request_about_another_host_than_the_certificates_is_refused() {
    let rig = Rig::start("host_cert_mismatch", Options::FILES).await;
    let client = rig.client(Some(&rig.ca.issue(&Spec::host("farm-1"))));
    let welcome = client.hello(None, &hello_body("farm-1")).await.unwrap();
    let own_beat = beat_of(&welcome, "farm-1");

    let hello = client.hello(None, &hello_body("farm-2")).await.unwrap();
    let mut beat: Value = own_beat;
    beat["host_id"] = json!("farm-2");
    let heartbeat = client.beat(None, &beat).await.unwrap();
    let read = client.host(None, "farm-2").await.unwrap();
    let cordon = client
        .send("POST", "/v1/hosts/farm-2:cordon", None, Some(&json!({})))
        .await
        .unwrap();
    let uncordon = client
        .send("POST", "/v1/hosts/farm-2:uncordon", None, None)
        .await
        .unwrap();
    for reply in [hello, heartbeat, read, cordon, uncordon] {
        assert_eq!(reply.status, 403, "{}", reply.body);
        assert_eq!(
            reply.json(),
            json!({ "error": "certificate-host-mismatch" })
        );
    }
    let listed = rig.listing().await;
    assert!(!listed.to_string().contains("farm-2"), "{listed}");
}

/// Asserts that with the CA configured the server serves, on the same listener, a token agent
/// (hello and heartbeat with the pool's join token and no certificate), a lease client (the
/// host list with the client token) and the health route, none presenting a certificate; and
/// that the pool's token is not an own-host credential, as before.
#[tokio::test]
async fn token_agents_and_lease_clients_need_no_certificate() {
    let rig = Rig::start("host_cert_tokens", Options::FILES).await;
    let client = rig.client(None);
    let hello = client
        .hello(Some(JOIN), &hello_body("farm-9"))
        .await
        .unwrap();
    assert_eq!(hello.status, 200, "{}", hello.body);
    let beat = client
        .beat(Some(JOIN), &beat_of(&hello, "farm-9"))
        .await
        .unwrap();
    assert_eq!(beat.status, 200, "{}", beat.body);
    let hosts = client
        .send("GET", "/v1/hosts", Some(TOKEN), None)
        .await
        .unwrap();
    assert_eq!(hosts.status, 200, "{}", hosts.body);
    assert!(hosts.body.contains("farm-9"), "{}", hosts.body);
    let health = client.send("GET", "/healthz", None, None).await.unwrap();
    assert_eq!((health.status, health.body.as_str()), (200, "ok"));
    let own = client.host(Some(JOIN), "farm-9").await.unwrap();
    assert_eq!(code(&own), "host-credential-required");
    let bare = client.hello(None, &hello_body("farm-9")).await.unwrap();
    assert_eq!(bare.status, 401);
}

/// Asserts that the CA's certificates are trusted whichever of the two a leaf was issued from:
/// the server started with a CA file of the current and the next root admits the agents of both.
#[tokio::test]
async fn a_ca_file_of_two_roots_admits_the_leaves_of_both() {
    let rig = Rig::start("host_cert_two_roots", Options::FILES).await;
    let next = Ca::new();
    let both = format!("{}{}", rig.ca.pem(), next.pem());
    rig.h.scratch.write("hostkeeper-ca.pem", &both, 0o644);
    rig.h.running.reload_tls().unwrap();
    for (host, ca) in [("farm-1", &rig.ca), ("farm-2", &next)] {
        let client = rig.client(Some(&ca.issue(&Spec::host(host))));
        let hello = client.hello(None, &hello_body(host)).await.unwrap();
        assert_eq!(hello.status, 200, "{host}: {}", hello.body);
    }
}
