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

/// Asserts that a certificate hello the registry refuses after the new binding was committed
/// (it names a pool the server does not serve) puts the token's binding back by a second
/// commit, so that the store and the registry hold the token's binding as before.
#[tokio::test]
async fn a_refused_rebinding_goes_back_to_the_token_binding() {
    let rig = rig("rebind_undo");
    rig.pooled("host-a", "burst").await.unwrap();
    rig.clock.advance(Duration::from_secs(60));
    let before = rig.commits();
    let refused = certified(&rig, "host-a", "elsewhere").await.unwrap_err();
    assert_eq!(
        (refused.status.as_u16(), refused.error),
        (403, "unknown-pool")
    );
    assert_eq!(rig.commits(), before + 2);
    assert_eq!(stored_credential(&rig, "host-a"), rig.pool.to_hex());
    assert_eq!(rig.held("host-a").unwrap().credential(), &rig.pool);
}
