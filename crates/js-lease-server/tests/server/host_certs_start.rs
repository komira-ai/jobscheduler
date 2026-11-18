//! `--agent-client-ca` at start and at reload: what the server refuses
//! to start with, the keys of the configuration file, and what a reload of the CA keeps and
//! takes.

use js_lease_server::{SharedClock, channel_ticker, start};
use serde_json::json;

use crate::agents_support::{agent_args, hello_body};
use crate::config_support::{PoolText, file, load, quoted, refusal, server};
use crate::host_certs_support::{
    Ca, Client, Options, Rig, START_OF_2030_MS, Spec, args as rig_args,
};
use crate::support::{Harness, Scratch, config};
use crate::tls::test_identity;

/// The refusal of a start over `args`, which must pass the checks of the flags: its flag and
/// its text.
async fn start_refused(args: Vec<std::ffi::OsString>) -> (&'static str, String) {
    let (_, ticker) = channel_ticker();
    let clock = SharedClock::new(js_types::ManualClock::default());
    let err = start(config(args).unwrap(), clock, ticker)
        .await
        .err()
        .unwrap();
    (err.flag(), err.to_string())
}

/// Asserts that `--agent-client-ca` without `--tls-cert` and `--tls-key` is refused naming the
/// flag, before anything starts, and that the file key is refused the same way, naming the key.
#[tokio::test]
async fn a_client_ca_without_tls_is_refused() {
    let scratch = Scratch::new("host_cert_no_tls");
    let ca = scratch.write("hostkeeper-ca.pem", &Ca::new().pem(), 0o644);
    let mut args = agent_args(&scratch);
    args.push("--agent-client-ca".into());
    args.push(ca.clone().into_os_string());
    let err = config(args).err().unwrap();
    assert_eq!(
        err.to_string(),
        "--agent-client-ca: requires --tls-cert and --tls-key: a certificate is asked for over TLS only"
    );

    let pool = PoolText::joined("hosts");
    let extra = format!("agent_client_ca = {}", quoted(&ca));
    let text = file(&server(&scratch, &extra), &[&pool]);
    assert_eq!(
        refusal(&scratch, &text),
        "server.agent_client_ca: requires server.tls_cert and server.tls_key: a certificate is asked for over TLS only"
    );
}

/// Asserts that a server whose CA file is absent, empty, holds a certificate block that is no
/// certificate does not start, and that each refusal names its flag and file.
#[tokio::test]
async fn an_unusable_ca_file_stops_the_start() {
    let junk = "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n";
    for (ca, flag) in [
        (None, "--agent-client-ca"),
        (Some(""), "--agent-client-ca"),
        (Some(junk), "--agent-client-ca"),
    ] {
        let scratch = Scratch::new("host_cert_bad_ca");
        let ca_file = scratch.path("hostkeeper-ca.pem");
        if let Some(text) = ca {
            scratch.write("hostkeeper-ca.pem", text, 0o644);
        }
        let args = rig_args(&scratch, Options::FILES, &ca_file);
        let (got, said) = start_refused(args).await;
        assert_eq!(got, flag);
        assert!(
            said.starts_with(&format!("{flag}: {}: ", ca_file.display())),
            "{said}"
        );
    }
}

/// Asserts that a CA file that fails at a reload (it is gone, then it holds no certificate)
/// leaves the CA in force, answering the reload with an error that names the flag: an agent of
/// the old CA is still admitted. It also asserts that the pair of the server is not swapped
/// alone when its CA fails: the certificate the clients trust stays the one served.
#[tokio::test]
async fn a_ca_reload_that_fails_keeps_the_ca_and_the_pair() {
    let rig = Rig::start("host_cert_reload_fails", Options::FILES).await;
    let old = rig.client(Some(&rig.ca.issue(&Spec::host("farm-1"))));
    let admitted = old.hello(None, &hello_body("farm-1")).await.unwrap();
    assert_eq!(admitted.status, 200, "{}", admitted.body);

    let (cert, key) = test_identity();
    rig.h.scratch.write("test-only-cert.pem", &cert, 0o644);
    rig.h.scratch.write("test-only-key.pem", &key, 0o600);
    std::fs::remove_file(&rig.ca_file).unwrap();
    assert_eq!(
        rig.h.running.reload_tls().unwrap_err().flag(),
        "--agent-client-ca"
    );
    rig.h.scratch.write("hostkeeper-ca.pem", "", 0o644);
    assert_eq!(
        rig.h.running.reload_tls().unwrap_err().flag(),
        "--agent-client-ca"
    );

    let still = rig.client(Some(&rig.ca.issue(&Spec::host("farm-2"))));
    let hello = still.hello(None, &hello_body("farm-2")).await.unwrap();
    assert_eq!(hello.status, 200, "{}", hello.body);
    let renewed = Client {
        trusted: cert,
        ..rig.client(None)
    };
    assert!(renewed.send("GET", "/healthz", None, None).await.is_err());
}

/// Asserts that a reload with a CA file that holds another CA trusts that CA from the next
/// handshake on and no longer the old one, and that it takes the renewed pair of the server in
/// the same reload.
#[tokio::test]
async fn a_ca_reload_that_succeeds_trusts_the_new_ca_alone() {
    let rig = Rig::start("host_cert_reload", Options::FILES).await;
    let next = Ca::new();
    let (cert, key) = test_identity();
    rig.h.scratch.write("hostkeeper-ca.pem", &next.pem(), 0o644);
    rig.h.scratch.write("test-only-cert.pem", &cert, 0o644);
    rig.h.scratch.write("test-only-key.pem", &key, 0o600);
    rig.h.running.reload_tls().unwrap();

    let trusting = |identity| Client {
        trusted: cert.clone(),
        ..rig.client(Some(identity))
    };
    let old = rig.ca.issue(&Spec::host("farm-1"));
    assert!(
        trusting(&old)
            .hello(None, &hello_body("farm-1"))
            .await
            .is_err()
    );
    let new = next.issue(&Spec::host("farm-1"));
    let hello = trusting(&new)
        .hello(None, &hello_body("farm-1"))
        .await
        .unwrap();
    assert_eq!(hello.status, 200, "{}", hello.body);
}

/// Asserts that a client that keeps one TLS configuration (so that it would resume the session
/// of an earlier connection if the server let it) is admitted with its certificate of the CA
/// in force, and after the server's CA file is replaced by another CA and reloaded, is refused
/// at its next connection with the same certificate: a resumed session would restore the
/// certificate it showed before without checking it against the CA again. It catches a server
/// that offers resumption on the listener that asks for client certificates.
#[tokio::test]
async fn a_client_that_could_resume_is_checked_again_after_the_ca_changed() {
    let rig = Rig::start("host_cert_no_resume", Options::FILES).await;
    let client = rig.client(Some(&rig.ca.issue(&Spec::host("farm-1"))));
    let session = client.connector();
    let welcome = client
        .send_with(
            &session,
            "POST",
            "/v1/agent/hello",
            None,
            Some(&hello_body("farm-1")),
        )
        .await
        .unwrap();
    assert_eq!(welcome.status, 200, "{}", welcome.body);
    let again = client
        .send_with(&session, "GET", "/v1/hosts/farm-1", None, None)
        .await
        .unwrap();
    assert_eq!(again.status, 200, "{}", again.body);

    rig.h
        .scratch
        .write("hostkeeper-ca.pem", &Ca::new().pem(), 0o644);
    rig.h.running.reload_tls().unwrap();
    let refused = client
        .send_with(&session, "GET", "/v1/hosts/farm-1", None, None)
        .await;
    assert!(refused.is_err(), "{refused:?}");
}

/// Asserts that a reload whose TLS pair fails (a key that is not the certificate's, a key file
/// that is not owner-only) while the CA file was replaced answers the error of the pair, naming
/// its flag, and keeps the old CA in force: a certificate of the old CA is still admitted and
/// one of the new CA is refused.
#[tokio::test]
async fn a_pair_that_fails_to_reload_keeps_the_old_ca_in_force() {
    let rig = Rig::start("host_cert_pair_fails", Options::FILES).await;
    let key_file = rig.h.scratch.path("test-only-key.pem");
    let (_, key) = test_identity();
    let other_key = rcgen::KeyPair::generate().unwrap().serialize_pem();
    let next = Ca::new();
    rig.h.scratch.write("hostkeeper-ca.pem", &next.pem(), 0o644);
    let shown = key_file.display();
    let uid = std::os::unix::fs::MetadataExt::uid(&std::fs::metadata(&key_file).unwrap());
    for (round, (text, mode, said)) in [
        (
            other_key,
            0o600,
            format!("--tls-key: {shown}: keys may not be consistent: KeyMismatch"),
        ),
        (
            key,
            0o644,
            format!("--tls-key: {shown} must be a regular file owned by the user running the server and readable only by that user (mode 0600 or 0400), not mode 0644 owned by uid {uid}"),
        ),
    ]
    .into_iter()
    .enumerate()
    {
        rig.h.scratch.write("test-only-key.pem", &text, mode);
        let err = rig.h.running.reload_tls().unwrap_err();
        assert_eq!((err.flag(), err.to_string()), ("--tls-key", said));

        let old_host = format!("farm-{}", 2 * round + 1);
        let old = rig.client(Some(&rig.ca.issue(&Spec::host(&old_host))));
        let admitted = old.hello(None, &hello_body(&old_host)).await.unwrap();
        assert_eq!(admitted.status, 200, "{}", admitted.body);
        let new_host = format!("farm-{}", 2 * round + 2);
        let new = rig.client(Some(&next.issue(&Spec::host(&new_host))));
        assert!(new.hello(None, &hello_body(&new_host)).await.is_err());
    }
}

/// Asserts that a configuration file with `agent_client_ca` serves the certificate of an agent
/// on the agent listener: the file's key is the flag's.
#[tokio::test]
async fn the_configuration_file_names_the_ca() {
    let scratch = Scratch::new("host_cert_file");
    let ca = Ca::new();
    let ca_file = scratch.write("hostkeeper-ca.pem", &ca.pem(), 0o644);
    let (cert, key) = test_identity();
    let tls_cert = scratch.write("cert.pem", &cert, 0o644);
    let tls_key = scratch.write("key.pem", &key, 0o600);
    let extra = format!(
        "tls_cert = {}\ntls_key = {}\nagent_client_ca = {}",
        quoted(&tls_cert),
        quoted(&tls_key),
        quoted(&ca_file)
    );
    let text = file(&server(&scratch, &extra), &[&PoolText::joined("hosts")]);
    let config = load(&scratch, &text).unwrap();
    let h = Harness::over(scratch, config).await;
    h.clock
        .set(js_types::Timestamp::from_millis(START_OF_2030_MS));
    let agent = h.running.agent_addr().unwrap();
    let hello = |host: &str| {
        let mut body = hello_body(host);
        body["pool"] = json!("hosts");
        body["shape"] = json!({ "cpu_millis": 8000, "memory_bytes": 17179869184u64, "gpus": 0 });
        body["capabilities"] = json!({ "arch": "x86_64" });
        body
    };
    let client = |host: &str| Client {
        addr: agent,
        trusted: cert.clone(),
        identity: Some(ca.issue(&Spec::host(host))),
        tls12: false,
    };
    let admitted = client("farm-1")
        .hello(None, &hello("farm-1"))
        .await
        .unwrap();
    assert_eq!(admitted.status, 200, "{}", admitted.body);
}
