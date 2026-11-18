//! The agent's own transport against the server: js-agent's [`Dialer`] logging in with the
//! host's certificate and key, kept where hostkeeper keeps them (the key readable by a group,
//! mode 0640), talking to a server that trusts hostkeeper's CA by `--agent-client-ca`. The two
//! sides agree on the certificate's shape, on the key's permissions and on what the certificate
//! is the credential for.

use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::Path;

use js_agent::Login;
use js_agent::{Answer, Connector, Dialer};
use serde_json::json;

use crate::agents_support::{entry, hello_body};
use crate::host_certs_support::{Ca, Identity, Options, Rig, Spec};

/// Puts `identity` into the directory `dir` as hostkeeper renews it in place: each file written
/// beside its place and renamed over it, the key readable by its group and by no one else.
fn place(dir: &Path, identity: &Identity) {
    for (name, text, mode) in [
        ("host.crt", &identity.cert_pem, 0o644),
        ("host.key", &identity.key_pem, 0o640),
    ] {
        let next = dir.join(format!("{name}.next"));
        fs::write(&next, text).unwrap();
        fs::set_permissions(&next, fs::Permissions::from_mode(mode)).unwrap();
        fs::rename(&next, dir.join(name)).unwrap();
    }
}

/// Asserts that the agent's own transport, given the host's certificate and key (the key mode
/// 0640) and no token anywhere, says hello and beats over mutual TLS; that after the pair is
/// renewed in place (a new key and serial, issued by a new CA) and the server trusts the new CA
/// alone, the same transport, with no restart, beats on in the session its hello opened, which
/// is possible only if it presented the renewed pair (the server refuses the old one from then
/// on); and that the server issued no host credential.
#[tokio::test]
async fn the_agents_transport_logs_in_with_the_host_certificate_and_its_renewal() {
    let rig = Rig::start("host_cert_agent", Options::FILES).await;
    let dir = rig.h.scratch.path("identity");
    fs::DirBuilder::new().mode(0o750).create(&dir).unwrap();
    place(&dir, &rig.ca.issue(&Spec::host("farm-1")));
    let ca_cert = rig.h.scratch.path("test-only-cert.pem");
    let server = format!("https://127.0.0.1:{}", rig.addr().port());

    let login =
        Login::host_certificate(Some(&dir.join("host.crt")), Some(&dir.join("host.key"))).unwrap();
    let next = Ca::new();
    let renewal = next.issue(&Spec::host("farm-1").serial(2));
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
    // Between the beats the pair is renewed and the server moves to the new CA.
    first_seen.recv().await.unwrap();
    place(&dir, &renewal);
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
