//! The agent's own transport against the server: js-agent's [`Dialer`] with the service
//! identity of a state root, as hostkeeper places it, talking to a server that trusts the
//! identity's CA by `--agent-client-ca`. The two sides agree on the file names, on the
//! certificate's shape and on what the certificate is the credential for.

use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

use js_agent::Login;
use js_agent::{Answer, Connector, Dialer};
use serde_json::json;

use crate::agents_support::{entry, hello_body};
use crate::service_certs_support::{Ca, Identity, Options, Rig, Spec};

/// The names hostkeeper gives the identity's files in the agent's state root, written out here
/// rather than taken from js-client so that a rename on one side fails this test.
const SERVICE_CERT_FILE: &str = "service-identity.pem";
const SERVICE_KEY_FILE: &str = "service-identity.key";

/// Puts `identity` into the state root `root` as hostkeeper does: each file written beside
/// its place and renamed over it, the key owner-only.
fn place(root: &Path, identity: &Identity) {
    for (name, text, mode) in [
        (SERVICE_CERT_FILE, &identity.cert_pem, 0o644),
        (SERVICE_KEY_FILE, &identity.key_pem, 0o600),
    ] {
        let next = root.join(format!("{name}.next"));
        fs::write(&next, text).unwrap();
        fs::set_permissions(&next, fs::Permissions::from_mode(mode)).unwrap();
        fs::rename(&next, root.join(name)).unwrap();
    }
}

/// Asserts that the agent's own transport, given a state root that holds a service identity
/// and no token anywhere, says hello and beats over mutual TLS; that after hostkeeper renews
/// the identity in place (a new key and serial, issued by a new CA) and the server trusts the
/// new CA alone, the same transport, with no restart, beats on in the session its hello opened,
/// which is possible only if it presented the renewed pair (the old one is refused by the server
/// from then on); and that the server saw no host credential issued.
#[tokio::test]
async fn the_agents_transport_logs_in_with_the_identity_hostkeeper_places_and_its_renewal() {
    let rig = Rig::start("service_cert_agent", Options::FILES).await;
    let root: PathBuf = rig.h.scratch.path("agent-state");
    fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
    place(&root, &rig.ca.issue(&Spec::agent("farm-1")));
    let ca_cert = rig.h.scratch.path("test-only-cert.pem");
    let server = format!("https://127.0.0.1:{}", rig.addr().port());

    let login = Login::identity_of(&root).unwrap().unwrap();
    let next = Ca::new();
    let renewal = next.issue(&Spec::agent("farm-1").serial(2));
    let (first_done, mut first_seen) = tokio::sync::mpsc::channel::<()>(1);
    let (go, mut go_seen) = tokio::sync::mpsc::channel::<()>(1);
    // The transport blocks, so the agent's side runs off the async workers, start to end.
    let agent = tokio::task::spawn_blocking(move || {
        let mut transport = Dialer.connect(&server, &login, Some(&ca_cert)).unwrap();
        let welcome = transport
            .post("/v1/agent/hello", &hello_body("farm-1"))
            .unwrap();
        let beat = json!({
            "host_id": "farm-1",
            "epoch": welcome.body["epoch"],
            "session": welcome.body["session"],
            "phase": "serving",
        });
        let first = transport.post("/v1/agent/heartbeat", &beat).unwrap();
        first_done.blocking_send(()).unwrap();
        go_seen.blocking_recv().unwrap();
        let renewed = transport.post("/v1/agent/heartbeat", &beat).unwrap();
        vec![welcome, first, renewed]
    });
    // Between the beats hostkeeper renews the files and the server moves to the new CA.
    first_seen.recv().await.unwrap();
    place(&root, &renewal);
    rig.h.scratch.write("hostkeeper-ca.pem", &next.pem(), 0o644);
    rig.h.running.reload_tls().unwrap();
    go.send(()).await.unwrap();
    let answers: Vec<Answer> = agent.await.unwrap();
    for answer in &answers {
        assert_eq!(answer.status, 200, "{answer:?}");
    }
    assert!(answers[0].body.get("host_credential").is_none());
    let listed = rig.listing().await;
    let host = entry(&listed, "farm-1");
    assert_eq!(host["state"], "live", "{host}");
    assert_eq!(host["lost"], false, "{host}");
}
