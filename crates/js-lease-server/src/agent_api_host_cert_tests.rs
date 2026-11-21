//! The hello of a host with a host certificate over a host bound to a join token, with the
//! bindings kept in a store: what is committed, when, and what goes back when the hello fails.

use super::super::host_cert::CertifiedHost;
use super::*;

/// The hello of `host` in `pool` with the host certificate that names `host`.
async fn certified(rig: &Rig, host: &str, pool: &str) -> Result<Welcome, Refused> {
    let id = HostId::new(host).unwrap();
    let who = Credential::of_host_certificate(&id);
    let certified = CertifiedHost(id);
    let given = (who, None, None, Some(&certified));
    let welcome =
        super::super::serve_hello_in(&rig.agents, given, Ok(body(host, pool, false)), &|| ());
    welcome.await.map(|Json(welcome)| welcome)
}

/// The credential a binding of `host` is stored with, as hex.
fn stored_credential(rig: &Rig, host: &str) -> String {
    rig.binding(host)["credential"].as_str().unwrap().to_owned()
}

/// Asserts that a hello with a certificate over a host bound to the pool's join token that is
/// not live commits the new binding in one transaction before the registry takes it: when
/// that commit fails (once, by the store's fault hook) the hello is refused 503
/// `state-write-failed` and the store and the registry both still hold the token's binding;
/// once the fault is gone the same hello is admitted with one more commit, and the store and
/// the registry hold the certificate's credential.
#[tokio::test]
async fn a_rebinding_is_committed_before_the_registry_takes_it() {
    let rig = rig("rebind_commit");
    rig.pooled("host-a", "burst").await.unwrap();
    rig.clock.advance(Duration::from_secs(60));
    let before = rig.commits();
    rig.store.fail_next();
    let failed = certified(&rig, "host-a", "burst").await.unwrap_err();
    assert_eq!(
        (failed.status.as_u16(), failed.error),
        (503, STATE_WRITE_FAILED)
    );
    assert_eq!(rig.commits(), before);
    assert_eq!(stored_credential(&rig, "host-a"), rig.pool.to_hex());
    assert_eq!(rig.held("host-a").unwrap().credential(), &rig.pool);

    certified(&rig, "host-a", "burst").await.unwrap();
    let id = HostId::new("host-a").unwrap();
    let cert = Credential::of_host_certificate(&id);
    assert_eq!(rig.commits(), before + 1);
    assert_eq!(stored_credential(&rig, "host-a"), cert.to_hex());
    assert_eq!(rig.held("host-a").unwrap().credential(), &cert);
}

/// Asserts that a host bound to the pool's join token that is live is refused a certificate
/// hello 409 `host-live` with nothing written, and keeps the token's binding.
#[tokio::test]
async fn a_live_token_host_is_not_rebound_and_nothing_is_written() {
    let rig = rig("rebind_live");
    rig.pooled("host-a", "burst").await.unwrap();
    let before = rig.commits();
    let refused = certified(&rig, "host-a", "burst").await.unwrap_err();
    assert_eq!((refused.status.as_u16(), refused.error), (409, "host-live"));
    assert_eq!(rig.commits(), before);
    assert_eq!(stored_credential(&rig, "host-a"), rig.pool.to_hex());
}

/// Asserts that a certificate hello the registry refuses (it names a pool the server does not
/// serve) is judged before the new binding is written: the store takes no commit, and the store
/// and the registry hold the token's binding as before.
#[tokio::test]
async fn a_refused_rebinding_writes_nothing_and_keeps_the_token_binding() {
    let rig = rig("rebind_undo");
    rig.pooled("host-a", "burst").await.unwrap();
    rig.clock.advance(Duration::from_secs(60));
    let before = rig.commits();
    let refused = certified(&rig, "host-a", "elsewhere").await.unwrap_err();
    assert_eq!(
        (refused.status.as_u16(), refused.error),
        (403, "unknown-pool")
    );
    assert_eq!(rig.commits(), before);
    assert_eq!(stored_credential(&rig, "host-a"), rig.pool.to_hex());
    assert_eq!(rig.held("host-a").unwrap().credential(), &rig.pool);
}

/// Asserts that a certificate hello from the process of a live host bound to the pool's join
/// token (the registry takes it for the same agent coming back, so it is not `host-live`) moves
/// the stored binding onto the certificate's credential as the registry moves its own: the store
/// and the registry hold the certificate's credential. It catches a hello that decides in the
/// store whether the host is idle, which leaves the store bound to the token while the registry
/// is bound to the certificate (a restart would then refuse the host).
#[tokio::test]
async fn a_certificate_hello_of_the_live_hosts_own_process_moves_the_stored_binding() {
    let rig = rig("rebind_same_process");
    let with_process = |host: &str| {
        let mut hello: Value = serde_json::from_slice(&body(host, "burst", false)).unwrap();
        hello["agent_process"] = json!("process-1");
        Bytes::from(hello.to_string())
    };
    let first = Ok(with_process("host-a"));
    let welcome = super::super::serve_hello(&rig.agents, rig.pool.clone(), None, first, &|| ());
    drop(welcome.await.unwrap());

    let id = HostId::new("host-a").unwrap();
    let who = Credential::of_host_certificate(&id);
    let certified = CertifiedHost(id);
    let given = (who.clone(), None, None, Some(&certified));
    let again = Ok(with_process("host-a"));
    let welcome = super::super::serve_hello_in(&rig.agents, given, again, &|| ());
    drop(welcome.await.expect("the same process is admitted"));
    assert_eq!(stored_credential(&rig, "host-a"), who.to_hex());
    assert_eq!(rig.held("host-a").unwrap().credential(), &who);
}
