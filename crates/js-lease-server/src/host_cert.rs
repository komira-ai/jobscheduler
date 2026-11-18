//! The shape of a host certificate, and the host it names.
//!
//! A certificate a host's client presents (see `--agent-client-ca`) is accepted only when it
//! has the shape hostkeeper's CA issues to a host: an end-entity certificate with exactly one
//! DNS SAN, which is a valid host id, and the extended key usage `clientAuth` and nothing
//! else. URI SANs (hostkeeper's role URIs among them) and the subject CN are not read: no
//! service name is required. Whether it chains to the CA is the TLS verifier's check
//! ([`crate::client_verifier`]); this module reads the one certificate it is given.

use js_types::HostId;
use x509_parser::extensions::GeneralName;
use x509_parser::parse_x509_certificate;

/// The host a connection's client certificate names, as the connection carries it for every
/// request it serves.
#[derive(Debug, Clone)]
pub(crate) struct CertHost(pub(crate) HostId);

/// Why a certificate is not a host certificate.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub(crate) enum CertFault {
    /// The bytes are not a certificate, or an extension of it cannot be read.
    #[error("the certificate cannot be read")]
    Unreadable,
    /// The certificate is a CA's.
    #[error("the certificate is a CA's, not an end entity's")]
    Authority,
    /// The certificate has no DNS SAN, or more than one.
    #[error("the certificate must have exactly one DNS name")]
    HostCount,
    /// The DNS SAN is not a host id.
    #[error("the DNS name of the certificate is not a valid host id")]
    HostId,
    /// The extended key usage is absent or holds more than `clientAuth`.
    #[error("the certificate's extended key usage must be clientAuth and nothing else")]
    KeyUsage,
}

/// What the shape rules read of a certificate.
struct Facts {
    /// Whether the certificate is a CA's.
    authority: bool,
    /// Whether its extended key usage is `clientAuth` and nothing else.
    client_only: bool,
    /// Its DNS SANs.
    dns: Vec<String>,
}

/// What `der` holds, or `None` when it is no certificate or an extension of it cannot be read.
fn read(der: &[u8]) -> Option<Facts> {
    let (_, cert) = parse_x509_certificate(der).ok()?;
    let authority = cert
        .basic_constraints()
        .ok()?
        .is_some_and(|constraints| constraints.value.ca);
    let client_only = cert.extended_key_usage().ok()?.is_some_and(|usage| {
        let usage = usage.value;
        let others = [
            usage.any,
            usage.server_auth,
            usage.code_signing,
            usage.email_protection,
            usage.time_stamping,
            usage.ocsp_signing,
            !usage.other.is_empty(),
        ];
        usage.client_auth && !others.into_iter().any(|set| set)
    });
    let names = cert.subject_alternative_name().ok()?;
    let names = names.map_or_else(Vec::new, |san| san.value.general_names.clone());
    let dns = names
        .into_iter()
        .filter_map(|name| match name {
            GeneralName::DNSName(name) => Some(name.to_owned()),
            _ => None,
        })
        .collect();
    Some(Facts {
        authority,
        client_only,
        dns,
    })
}

/// The host the host certificate `der` names, or why it is not one.
pub(crate) fn host_of(der: &[u8]) -> Result<HostId, CertFault> {
    let facts = read(der).ok_or(CertFault::Unreadable)?;
    if facts.authority {
        return Err(CertFault::Authority);
    }
    if !facts.client_only {
        return Err(CertFault::KeyUsage);
    }
    let [host] = &facts.dns[..] else {
        return Err(CertFault::HostCount);
    };
    HostId::new(host).map_err(|_| CertFault::HostId)
}

#[cfg(test)]
#[path = "host_cert_tests.rs"]
mod tests;
