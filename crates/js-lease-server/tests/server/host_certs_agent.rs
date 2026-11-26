//! The agent's own transport against the server: js-agent's [`Dialer`] logging in with the
//! host's certificate and key, kept where hostkeeper keeps them (the key readable by a group,
//! mode 0640), talking to a server that trusts hostkeeper's CA by `--agent-client-ca`. The two
//! sides agree on the certificate's shape, on the key's permissions and on what the certificate
//! is the credential for, and a host that joined by the pool's token moves onto its certificate
//! while the hosts still on the token carry on.

use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::Path;
use std::time::Duration;

use js_agent::Login;
use js_agent::{Answer, Connector, Dialer};
use js_registry::Credential;
use js_types::HostId;
use serde_json::{Value, json};

use crate::agents_support::{JOIN, entry, hello_body};
use crate::host_certs_support::{Ca, Identity, Options, Rig, Spec};
use crate::support::{Kept, kept_bindings};

/// The agent route of a hello.
const HELLO: &str = "/v1/agent/hello";
/// The agent route of a heartbeat.
const BEAT: &str = "/v1/agent/heartbeat";

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

/// Connects js-agent's transport to `rig` with `login`, as an agent process does at its start,
/// and posts `body` to `path`. A hello or a heartbeat carries its epoch and session in its body,
/// so one connection per request is what an agent restarted with other flags sends.
async fn post(rig: &Rig, login: &Login, path: &'static str, body: Value) -> Answer {
    let server = format!("https://127.0.0.1:{}", rig.addr().port());
    let ca_cert = rig.h.scratch.path("test-only-cert.pem");
    let login = login.clone();
    // The transport blocks, so it runs off the async workers.
    tokio::task::spawn_blocking(move || {
        let mut transport = Dialer.connect(&server, &login, Some(&ca_cert)).unwrap();
        transport.post(path, &body).unwrap()
    })
    .await
    .unwrap()
}

/// The heartbeat of the session `welcome` opened for `host`.
fn beat(host: &str, welcome: &Answer) -> Value {
    assert_eq!(welcome.status, 200, "{welcome:?}");
    json!({
        "host_id": host,
        "epoch": welcome.body["epoch"],
        "session": welcome.body["session"],
        "phase": "serving",
    })
}

/// The status of `answer` and the code of its refusal (empty when it has none).
fn refusal(answer: &Answer) -> (u16, &str) {
    (
        answer.status,
        answer.body["error"].as_str().unwrap_or_default(),
    )
}

/// Asserts, with the bindings in files and in the store, the move of a host that joined by the
/// pool's token (`--token-file`, as a Mac does before its flags name its certificate) onto its
/// host certificate, js-agent's own transport making every request of both logins. The host
/// says hello and beats on the token. Restarted with its certificate while its token session is
/// live, its hello is refused `host-live`. Once that session has lapsed, the certificate's hello
/// is admitted at the next epoch with no host credential, it beats, and the kept binding holds
/// the certificate's credential; the token's hello for that host is then refused `host-bound`.
/// Beside it a second host on the same token says hello before the move and again after it, and
/// beats, so the token path keeps working for the hosts that have not moved.
#[tokio::test]
async fn a_token_host_moves_onto_its_certificate_while_another_stays_on_the_token() {
    for kept in Kept::BOTH {
        let options = Options {
            kept,
            ..Options::FILES
        };
        let rig = Rig::start("host_cert_agent_move", options).await;
        let token = Login::Token(
            rig.h
                .scratch
                .write("join-token", &format!("{JOIN}\n"), 0o600),
        );
        let dir = rig.h.scratch.path("identity");
        fs::DirBuilder::new().mode(0o750).create(&dir).unwrap();
        place(&dir, &rig.ca.issue(&Spec::host("mac-1")));
        let certificate =
            Login::host_certificate(Some(&dir.join("host.crt")), Some(&dir.join("host.key")))
                .unwrap();

        let on_token = post(&rig, &token, HELLO, hello_body("mac-1")).await;
        assert_eq!(on_token.status, 200, "{kept:?}: {on_token:?}");
        let token_beat = post(&rig, &token, BEAT, beat("mac-1", &on_token)).await;
        assert_eq!(token_beat.status, 200, "{kept:?}: {token_beat:?}");
        let other = post(&rig, &token, HELLO, hello_body("farm-9")).await;
        assert_eq!(other.status, 200, "{kept:?}: {other:?}");

        let live = post(&rig, &certificate, HELLO, hello_body("mac-1")).await;
        assert_eq!(refusal(&live), (409, "host-live"), "{kept:?}: {live:?}");

        rig.h.clock.advance(Duration::from_secs(60));
        let moved = post(&rig, &certificate, HELLO, hello_body("mac-1")).await;
        assert_eq!(moved.status, 200, "{kept:?}: {moved:?}");
        assert_eq!(moved.body["epoch"], 2, "{kept:?}: {moved:?}");
        assert!(
            moved.body.get("host_credential").is_none(),
            "{kept:?}: {moved:?}"
        );
        let cert_beat = post(&rig, &certificate, BEAT, beat("mac-1", &moved)).await;
        assert_eq!(cert_beat.status, 200, "{kept:?}: {cert_beat:?}");
        let certified = Credential::of_host_certificate(&HostId::new("mac-1").unwrap()).to_hex();
        let held = kept_bindings(&rig.h);
        assert!(held.contains(&certified), "{kept:?}: {held}");

        let taken = post(&rig, &token, HELLO, hello_body("mac-1")).await;
        assert_eq!(refusal(&taken), (403, "host-bound"), "{kept:?}: {taken:?}");

        let stays = post(&rig, &token, HELLO, hello_body("farm-9")).await;
        assert_eq!(stays.status, 200, "{kept:?}: {stays:?}");
        assert_eq!(stays.body["epoch"], 2, "{kept:?}: {stays:?}");
        let stays_beat = post(&rig, &token, BEAT, beat("farm-9", &stays)).await;
        assert_eq!(stays_beat.status, 200, "{kept:?}: {stays_beat:?}");

        let listed = rig.listing().await;
        for host in ["mac-1", "farm-9"] {
            let listed_host = entry(&listed, host);
            assert_eq!(listed_host["state"], "live", "{kept:?}: {listed_host}");
        }
    }
}
