//! The shape rules of a service certificate, one broken at a time.

use rcgen::ExtendedKeyUsagePurpose as Usage;

use super::*;
use crate::service_cert_mint::{AGENT_URI, Shape, self_signed};

/// The result of the shape rules over a certificate of `shape`.
fn read_shape(shape: &Shape) -> Result<HostId, CertFault> {
    host_of(self_signed(shape).der.as_ref())
}

/// Asserts that the certificate hostkeeper issues is read as the host its one DNS name is,
/// whatever other kinds of SAN it carries.
#[test]
fn a_service_certificate_names_its_host() {
    let host = read_shape(&Shape::agent("farm-1")).unwrap();
    assert_eq!(host.as_str(), "farm-1");
}

/// Asserts that bytes that are no certificate are refused as unreadable.
#[test]
fn bytes_that_are_no_certificate_are_unreadable() {
    assert_eq!(host_of(&[0x30, 0x00]), Err(CertFault::Unreadable));
    assert_eq!(host_of(&[]), Err(CertFault::Unreadable));
}

/// Asserts that a certificate that is a CA's is refused, even with every other rule met.
#[test]
fn a_certificate_authority_is_not_a_service_certificate() {
    let shape = Shape {
        ca: true,
        ..Shape::agent("farm-1")
    };
    assert_eq!(read_shape(&shape), Err(CertFault::Authority));
}

/// Asserts that the extended key usage must be clientAuth and nothing else: absent, serverAuth
/// beside it, any, and each other purpose are refused, with the message that says so.
#[test]
fn the_key_usage_must_be_client_auth_alone() {
    let refused = |usages: Vec<Usage>| {
        let shape = Shape {
            usages,
            ..Shape::agent("farm-1")
        };
        read_shape(&shape)
    };
    assert_eq!(refused(vec![]), Err(CertFault::KeyUsage));
    assert_eq!(refused(vec![Usage::ServerAuth]), Err(CertFault::KeyUsage));
    for other in [
        Usage::ServerAuth,
        Usage::Any,
        Usage::CodeSigning,
        Usage::EmailProtection,
        Usage::TimeStamping,
        Usage::OcspSigning,
        Usage::Other(vec![1, 3, 6, 1, 4, 1, 99999]),
    ] {
        assert_eq!(
            refused(vec![Usage::ClientAuth, other]),
            Err(CertFault::KeyUsage)
        );
    }
    assert_eq!(
        CertFault::KeyUsage.to_string(),
        "the certificate's extended key usage must be clientAuth and nothing else"
    );
}

/// Asserts that the certificate must name the agent service and no other hostkeeper service:
/// no URI, another service's, the agent's with another's beside it, and a service name that
/// only starts like the agent's are all refused; a URI that is no service's is ignored.
#[test]
fn the_certificate_must_name_the_agent_service_alone() {
    let uris = |uris: &[&str]| {
        let shape = Shape {
            uris: uris.iter().map(ToString::to_string).collect(),
            ..Shape::agent("farm-1")
        };
        read_shape(&shape)
    };
    assert_eq!(uris(&[]), Err(CertFault::Service));
    assert_eq!(
        uris(&["urn:hostkeeper:service:hostkeeper-agent"]),
        Err(CertFault::Service)
    );
    assert_eq!(
        uris(&[AGENT_URI, "urn:hostkeeper:service:other"]),
        Err(CertFault::Service)
    );
    assert_eq!(uris(&[AGENT_URI, AGENT_URI]), Err(CertFault::Service));
    assert_eq!(
        uris(&["urn:hostkeeper:service:js-agent-2"]),
        Err(CertFault::Service)
    );
    assert!(uris(&[AGENT_URI, "https://example.test/js-agent"]).is_ok());
}

/// Asserts that exactly one DNS name is required and that it must be a host id: none or two
/// are refused, as is a name outside the host id alphabet.
#[test]
fn the_certificate_must_have_one_dns_name_that_is_a_host_id() {
    let dns = |names: &[&str]| {
        let shape = Shape {
            dns: names.iter().map(ToString::to_string).collect(),
            ..Shape::agent("farm-1")
        };
        read_shape(&shape)
    };
    assert_eq!(dns(&[]), Err(CertFault::HostCount));
    assert_eq!(dns(&["farm-1", "farm-2"]), Err(CertFault::HostCount));
    assert_eq!(dns(&["Farm_1.EXAMPLE"]), Err(CertFault::HostId));
    assert_eq!(dns(&["farm-1"]).unwrap().as_str(), "farm-1");
}

/// Asserts that a certificate with no SAN extension at all names no service and is refused.
#[test]
fn a_certificate_without_names_is_refused() {
    let shape = Shape {
        dns: vec![],
        uris: vec![],
        ip: false,
        ..Shape::agent("farm-1")
    };
    assert_eq!(read_shape(&shape), Err(CertFault::Service));
}
