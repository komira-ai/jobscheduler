//! The shape rules of a host certificate, one broken at a time.

use rcgen::ExtendedKeyUsagePurpose as Usage;

use super::*;
use crate::host_cert_mint::{AGENT_ROLE_URI, HOST_ROLE_URI, Shape, self_signed};

/// The result of the shape rules over a certificate of `shape`.
fn read_shape(shape: &Shape) -> Result<HostId, CertFault> {
    host_of(self_signed(shape).der.as_ref())
}

/// Asserts that the certificate hostkeeper issues is read as the host its one DNS name is,
/// whatever other kinds of SAN it carries.
#[test]
fn a_host_certificate_names_its_host() {
    let host = read_shape(&Shape::host("farm-1")).unwrap();
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
fn a_certificate_authority_is_not_a_host_certificate() {
    let shape = Shape {
        ca: true,
        ..Shape::host("farm-1")
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
            ..Shape::host("farm-1")
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

/// Asserts that no URI SAN and no CN is required or read: the host-role URI, the agent-role
/// URI, any other URI, several URIs, none, a CN that names another host and no CN all give the
/// host of the one DNS name.
#[test]
fn uris_and_the_common_name_are_not_read() {
    let uris = |uris: &[&str]| {
        let shape = Shape {
            uris: uris.iter().map(ToString::to_string).collect(),
            ..Shape::host("farm-1")
        };
        read_shape(&shape)
    };
    for case in [
        &[][..],
        &[HOST_ROLE_URI],
        &[AGENT_ROLE_URI],
        &[HOST_ROLE_URI, AGENT_ROLE_URI],
        &["https://example.test/anything"],
    ] {
        assert_eq!(uris(case).unwrap().as_str(), "farm-1", "{case:?}");
    }
    for cn in [Some("host:farm-2".to_owned()), None] {
        let shape = Shape {
            cn,
            ..Shape::host("farm-1")
        };
        assert_eq!(read_shape(&shape).unwrap().as_str(), "farm-1");
    }
}

/// Asserts that exactly one DNS name is required and that it must be a host id: none or two
/// are refused, as is a name outside the host id alphabet.
#[test]
fn the_certificate_must_have_one_dns_name_that_is_a_host_id() {
    let dns = |names: &[&str]| {
        let shape = Shape {
            dns: names.iter().map(ToString::to_string).collect(),
            ..Shape::host("farm-1")
        };
        read_shape(&shape)
    };
    assert_eq!(dns(&[]), Err(CertFault::HostCount));
    assert_eq!(dns(&["farm-1", "farm-2"]), Err(CertFault::HostCount));
    assert_eq!(dns(&["Farm_1.EXAMPLE"]), Err(CertFault::HostId));
    assert_eq!(dns(&["farm-1"]).unwrap().as_str(), "farm-1");
}

/// Asserts that a certificate with no SAN extension at all names no host and is refused.
#[test]
fn a_certificate_without_names_is_refused() {
    let shape = Shape {
        dns: vec![],
        uris: vec![],
        ip: false,
        ..Shape::host("farm-1")
    };
    assert_eq!(read_shape(&shape), Err(CertFault::HostCount));
}
