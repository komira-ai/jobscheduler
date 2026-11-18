//! The client verifier: which CA files it takes, and which certificates it admits.

use std::path::PathBuf;

use js_testkit::dir::TestDir;
use rcgen::ExtendedKeyUsagePurpose as Usage;
use rustls::pki_types::UnixTime;

use super::*;
use crate::service_cert_mint::{Authority, Shape};

/// A scratch directory and the path of a file in it holding `text`.
fn file_with(text: &str) -> (TestDir, PathBuf) {
    let dir = TestDir::under(std::env::temp_dir(), "client_verifier_unit");
    let path = dir.path().join("ca.pem");
    std::fs::write(&path, text).unwrap();
    (dir, path)
}

/// The refusal of the CA file with `text`.
fn refusal(text: &str) -> String {
    let (_dir, path) = file_with(text);
    let err = ServiceVerifier::open(&path).unwrap_err();
    assert_eq!(err.flag(), "--agent-client-ca");
    err.to_string().replace(&path.display().to_string(), "<ca>")
}

/// Whether `verifier` admits the certificate `der` of a client.
fn admits(verifier: &ServiceVerifier, der: &CertificateDer<'static>) -> bool {
    verifier
        .verify_client_cert(der, &[], UnixTime::now())
        .is_ok()
}

/// Asserts that a certificate of the CA with the shape of an agent's service certificate is
/// admitted, that a certificate of another CA is not, and that one of the CA whose shape is
/// wrong (serverAuth beside clientAuth) is refused with the shape's own message.
#[test]
fn a_certificate_is_admitted_by_its_ca_and_its_shape() {
    let ca = Authority::new();
    let (_dir, path) = file_with(&ca.pem());
    let verifier = ServiceVerifier::open(&path).unwrap();
    assert!(admits(&verifier, &ca.issue(&Shape::agent("farm-1")).der));
    let stranger = Authority::new().issue(&Shape::agent("farm-1")).der;
    assert!(!admits(&verifier, &stranger));
    let both = Shape {
        usages: vec![Usage::ClientAuth, Usage::ServerAuth],
        ..Shape::agent("farm-1")
    };
    let refused = verifier
        .verify_client_cert(&ca.issue(&both).der, &[], UnixTime::now())
        .unwrap_err();
    let said = "the certificate's extended key usage must be clientAuth and nothing else";
    assert_eq!(refused, Error::General(said.to_owned()));
}

/// Asserts that a CA file of two certificates (current and next) trusts the leaves of both.
#[test]
fn a_ca_file_may_hold_two_roots() {
    let (current, next) = (Authority::new(), Authority::new());
    let (_dir, path) = file_with(&format!("{}{}", current.pem(), next.pem()));
    let verifier = ServiceVerifier::open(&path).unwrap();
    assert!(admits(
        &verifier,
        &current.issue(&Shape::agent("farm-1")).der
    ));
    assert!(admits(&verifier, &next.issue(&Shape::agent("farm-1")).der));
}

/// Asserts that a replacement trusts what the new file trusts and no longer what the old one
/// did.
#[test]
fn a_replacement_trusts_the_new_ca_alone() {
    let (old, new) = (Authority::new(), Authority::new());
    let (_dir, path) = file_with(&old.pem());
    let verifier = ServiceVerifier::open(&path).unwrap();
    let (_other, fresh) = file_with(&new.pem());
    verifier.replace(load(&fresh).unwrap());
    assert!(!admits(&verifier, &old.issue(&Shape::agent("farm-1")).der));
    assert!(admits(&verifier, &new.issue(&Shape::agent("farm-1")).der));
}

/// Asserts that a CA file with no certificate, one that is not PEM at all, one whose
/// certificate block is not a certificate, and one that is absent are refused naming the flag
/// and the file.
#[test]
fn a_ca_file_without_a_usable_certificate_is_refused() {
    assert_eq!(
        refusal(""),
        "--agent-client-ca: <ca>: no root trust anchors were provided"
    );
    assert_eq!(
        refusal("no certificate here\n"),
        "--agent-client-ca: <ca>: no root trust anchors were provided"
    );
    let junk = "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n";
    assert!(refusal(junk).starts_with("--agent-client-ca: <ca>: "));
    let (dir, _) = file_with("");
    let absent = dir.path().join("absent.pem");
    let err = ServiceVerifier::open(&absent).unwrap_err();
    assert!(
        err.to_string()
            .starts_with(&format!("--agent-client-ca: {}: ", absent.display()))
    );
}

/// Asserts what the verifier tells a client of itself: it asks for a certificate, does not
/// insist on one, and names no CA to choose from.
#[test]
fn the_verifier_asks_for_a_certificate_but_does_not_require_one() {
    let ca = Authority::new();
    let (_dir, path) = file_with(&ca.pem());
    let verifier = ServiceVerifier::open(&path).unwrap();
    assert!(verifier.offer_client_auth());
    assert!(!verifier.client_auth_mandatory());
    assert!(verifier.root_hint_subjects().is_empty());
    assert!(!verifier.supported_verify_schemes().is_empty());
}
