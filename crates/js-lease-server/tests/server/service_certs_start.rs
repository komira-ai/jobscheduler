//! `--agent-client-ca` and `--agent-deny-file` at start and at reload: what the server refuses
//! to start with, the keys of the configuration file, and what a reload of the CA keeps and
//! takes.

use js_lease_server::{SharedClock, channel_ticker, start};
use serde_json::json;

use crate::agents_support::{agent_args, hello_body};
use crate::config_support::{PoolText, file, load, quoted, refusal, server};
use crate::service_certs_support::{
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
    let scratch = Scratch::new("service_cert_no_tls");
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
/// certificate, or whose deny file is absent or has a line that is no host id, does not start,
/// and that each refusal names its flag and file (and, for the deny file, the number of the line
/// and nothing of its text).
#[tokio::test]
async fn an_unusable_ca_or_deny_file_stops_the_start() {
    let junk = "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n";
    for (ca, flag) in [
        (None, "--agent-client-ca"),
        (Some(""), "--agent-client-ca"),
        (Some(junk), "--agent-client-ca"),
    ] {
        let scratch = Scratch::new("service_cert_bad_ca");
        let ca_file = scratch.path("hostkeeper-ca.pem");
        if let Some(text) = ca {
            scratch.write("hostkeeper-ca.pem", text, 0o644);
        }
        let deny = scratch.write("deny", "", 0o644);
        let args = rig_args(&scratch, Options::FILES, &(ca_file.clone(), deny));
        let (got, said) = start_refused(args).await;
        assert_eq!(got, flag);
        assert!(
            said.starts_with(&format!("{flag}: {}: ", ca_file.display())),
            "{said}"
        );
    }

    let scratch = Scratch::new("service_cert_bad_deny");
    let ca = scratch.write("hostkeeper-ca.pem", &Ca::new().pem(), 0o644);
    let deny = scratch.write("deny", "farm-1\nsecret value here\n", 0o644);
    let args = rig_args(&scratch, Options::FILES, &(ca.clone(), deny.clone()));
    let (flag, said) = start_refused(args).await;
    assert_eq!(flag, "--agent-deny-file");
    assert_eq!(
        said,
        format!(
            "--agent-deny-file: {}: line 2: not a host id",
            deny.display()
        )
    );

    let args = rig_args(&scratch, Options::FILES, &(ca, scratch.path("absent")));
    let (flag, _) = start_refused(args).await;
    assert_eq!(flag, "--agent-deny-file");
}

/// Asserts that a CA file that fails at a reload (it is gone, then it holds no certificate)
/// leaves the CA in force, answering the reload with an error that names the flag: an agent of
/// the old CA is still admitted. It also asserts that the pair of the server is not swapped
/// alone when its CA fails: the certificate the clients trust stays the one served.
#[tokio::test]
async fn a_ca_reload_that_fails_keeps_the_ca_and_the_pair() {
    let rig = Rig::start("service_cert_reload_fails", Options::FILES).await;
    let old = rig.client(Some(&rig.ca.issue(&Spec::agent("farm-1"))));
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

    let still = rig.client(Some(&rig.ca.issue(&Spec::agent("farm-2"))));
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
    let rig = Rig::start("service_cert_reload", Options::FILES).await;
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
    let old = rig.ca.issue(&Spec::agent("farm-1"));
    assert!(
        trusting(&old)
            .hello(None, &hello_body("farm-1"))
            .await
            .is_err()
    );
    let new = next.issue(&Spec::agent("farm-1"));
    let hello = trusting(&new)
        .hello(None, &hello_body("farm-1"))
        .await
        .unwrap();
    assert_eq!(hello.status, 200, "{}", hello.body);
}

/// Asserts that a configuration file with `agent_client_ca` and `agent_deny_file` serves the
/// certificate of an agent on the agent listener and refuses a denied host there: the file's
/// keys are the flags'.
#[tokio::test]
async fn the_configuration_file_names_the_ca_and_the_deny_file() {
    let scratch = Scratch::new("service_cert_file");
    let ca = Ca::new();
    let ca_file = scratch.write("hostkeeper-ca.pem", &ca.pem(), 0o644);
    let (cert, key) = test_identity();
    let tls_cert = scratch.write("cert.pem", &cert, 0o644);
    let tls_key = scratch.write("key.pem", &key, 0o600);
    let deny = scratch.write("deny", "farm-2\n", 0o644);
    let extra = format!(
        "tls_cert = {}\ntls_key = {}\nagent_client_ca = {}\nagent_deny_file = {}",
        quoted(&tls_cert),
        quoted(&tls_key),
        quoted(&ca_file),
        quoted(&deny)
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
        identity: Some(ca.issue(&Spec::agent(host))),
        tls12: false,
    };
    let admitted = client("farm-1")
        .hello(None, &hello("farm-1"))
        .await
        .unwrap();
    assert_eq!(admitted.status, 200, "{}", admitted.body);
    let denied = client("farm-2")
        .hello(None, &hello("farm-2"))
        .await
        .unwrap();
    assert_eq!(denied.status, 403, "{}", denied.body);
    assert_eq!(denied.json(), json!({ "error": "host-denied" }));
}
