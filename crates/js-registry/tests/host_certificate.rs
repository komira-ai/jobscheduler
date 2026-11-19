//! The credential of a host certificate, and the hello that moves a host bound to a join
//! token over to it.

mod support;

use std::time::Duration;

use js_agent_wire::{InstanceIdentity, Taint};
use js_registry::{Credential, HostState, Refusal};
use js_types::cordon::Asker;
use support::*;

/// The credential of the certificate that names `name`.
fn certified(name: &str) -> Credential {
    Credential::of_host_certificate(&host(name))
}

/// Says hello for `name` as a host that presents its host certificate.
fn hello_certified(fx: &Fixture, name: &str) -> Result<js_agent_wire::Welcome, Refusal> {
    fx.registry
        .hello_as_certified_host(&hello(name), &certified(name))
}

/// Asserts what makes the credential of a certificate a credential of a host: the same host
/// gives the same credential whatever the certificate's key, another host gives another, no
/// join token (not even the host id taken as one) is it, and it survives the hex form the
/// bindings are stored in.
#[test]
fn a_host_certificate_credential_belongs_to_its_host_alone() {
    assert_eq!(certified("a"), certified("a"));
    assert_ne!(certified("a"), certified("b"));
    assert_ne!(certified("a"), cred("a"));
    let text = certified("a").to_hex();
    assert_eq!(Credential::from_hex(&text), Some(certified("a")));
}

/// Asserts that a host nobody knows is bound to its certificate credential by a certificate
/// hello, and that a later certificate hello of it (after the grace) gets back in with the
/// next epoch.
#[test]
fn a_certificate_hello_binds_a_new_host_to_the_certificate_credential() {
    let fx = fixture(2);
    assert_eq!(hello_certified(&fx, "a").unwrap().epoch, 1);
    assert_eq!(
        fx.registry.bindings()[&host("a")].credential(),
        &certified("a")
    );
    fx.pass_grace();
    assert_eq!(hello_certified(&fx, "a").unwrap().epoch, 2);
}

/// Asserts that a host bound to a join token that is not live is bound to its certificate
/// credential by the certificate's first hello, with the taint and the cordon it had kept
/// and the epoch carried on; and that the join token is then refused as host-bound.
#[test]
fn a_certificate_hello_rebinds_a_lost_token_host_keeping_its_taint_and_cordon() {
    let fx = fixture(2);
    fx.join("a", "t");
    let taint = Taint {
        reason: "residue".into(),
        run_id: None,
        check: None,
    };
    assert!(fx.registry.taint_host(&host("a"), taint.clone()));
    fx.registry
        .cordon(&host("a"), Duration::from_secs(3600), &Asker::Unnamed)
        .unwrap();
    fx.pass_grace();

    assert_eq!(hello_certified(&fx, "a").unwrap().epoch, 2);

    let bound = fx.registry.bindings().remove(&host("a")).unwrap();
    assert_eq!(bound.credential(), &certified("a"));
    assert_eq!(bound.taint(), Some(&taint));
    assert!(bound.cordon_until_ms().is_some());
    fx.pass_grace();
    let token = fx.registry.hello(&hello("a"), &cred("t")).unwrap_err();
    assert_eq!(token, Refusal::HostBound);
}

/// Asserts that a host bound to a join token that is live keeps its binding against a
/// certificate hello, which is refused as host-live.
#[test]
fn a_certificate_hello_does_not_take_a_live_token_host() {
    let fx = fixture(2);
    fx.join("a", "t");
    let refused = hello_certified(&fx, "a").unwrap_err();
    assert_eq!(refused, Refusal::HostLive);
    assert_eq!(fx.registry.bindings()[&host("a")].credential(), &cred("t"));
    assert_eq!(fx.registry.hosts()[0].state, HostState::Live);
}

/// Asserts that a host bound to a certificate is never taken by a join token, live or lost.
#[test]
fn a_join_token_does_not_take_a_certificate_host() {
    let fx = fixture(2);
    hello_certified(&fx, "a").unwrap();
    assert_eq!(
        fx.registry.hello(&hello("a"), &cred("t")).unwrap_err(),
        Refusal::HostBound
    );
    fx.pass_grace();
    assert_eq!(
        fx.registry.hello(&hello("a"), &cred("t")).unwrap_err(),
        Refusal::HostBound
    );
}

/// Asserts that the instance of a token binding is replaced by the certificate hello's
/// (here none), and that a certificate host whose binding names an instance holds a later
/// hello of the same certificate to it, as any binding does.
#[test]
fn a_rebinding_takes_the_instance_of_the_certificate_hello() {
    let fx = fixture(2);
    let mut launched = hello("a");
    launched.instance = Some(InstanceIdentity {
        provider: "aws".into(),
        instance_id: "i-1".into(),
    });
    fx.registry.hello(&launched, &cred("t")).unwrap();
    fx.pass_grace();
    hello_certified(&fx, "a").unwrap();
    assert!(fx.registry.bindings()[&host("a")].instance().is_none());

    let mut named = hello("b");
    named.instance = Some(InstanceIdentity {
        provider: "aws".into(),
        instance_id: "i-2".into(),
    });
    fx.registry
        .hello_as_certified_host(&named, &certified("b"))
        .unwrap();
    fx.pass_grace();
    let elsewhere = fx
        .registry
        .hello_as_certified_host(&hello("b"), &certified("b"))
        .unwrap_err();
    assert_eq!(elsewhere, Refusal::HostBound);
}
