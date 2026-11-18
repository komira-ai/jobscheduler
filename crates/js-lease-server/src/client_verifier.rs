//! The verifier of the client certificates an agent presents (`--agent-client-ca`).
//!
//! A client may present no certificate (token agents and lease clients do not), but one that is
//! presented must chain to the CA file's certificates by `rustls`'s own web PKI verifier, and
//! must be a service certificate of an agent ([`crate::service_cert::host_of`]); otherwise the
//! handshake fails. The CA file's certificates are behind a lock, so that a reload replaces
//! them for the next handshake and a connection already open keeps what it negotiated.

use std::path::Path;
use std::sync::{Arc, PoisonError, RwLock};

use rustls::client::danger::HandshakeSignatureValid;
use rustls::crypto::ring::default_provider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, UnixTime};
use rustls::server::WebPkiClientVerifier;
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, Error, RootCertStore, SignatureScheme};

use crate::error::StartupError;
use crate::service_cert::host_of;

/// The flag the refusals name.
const FLAG: &str = "--agent-client-ca";

/// The verifier of the agent listeners' TLS: optional client certificates, trusted by the CA
/// file's certificates as of the last load.
#[derive(Debug)]
pub(crate) struct ServiceVerifier {
    inner: RwLock<Arc<dyn ClientCertVerifier>>,
}

impl ServiceVerifier {
    /// The verifier of the certificates in the PEM file `ca`.
    pub(crate) fn open(ca: &Path) -> Result<Self, StartupError> {
        Ok(Self {
            inner: RwLock::new(load(ca)?),
        })
    }

    /// Trusts what `fresh` (from [`load`]) trusts from the next handshake on, in place of what
    /// is trusted now.
    pub(crate) fn replace(&self, fresh: Arc<dyn ClientCertVerifier>) {
        *self.inner.write().unwrap_or_else(PoisonError::into_inner) = fresh;
    }

    /// The web PKI verifier in force.
    fn current(&self) -> Arc<dyn ClientCertVerifier> {
        self.inner
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// The web PKI verifier of the certificates in the PEM file `ca`, which lets a client present
/// none. A file with no certificate is refused, as one that cannot be read is.
pub(crate) fn load(ca: &Path) -> Result<Arc<dyn ClientCertVerifier>, StartupError> {
    let refuse =
        |why: &dyn std::fmt::Display| StartupError::new(FLAG, format!("{}: {why}", ca.display()));
    let certs = CertificateDer::pem_file_iter(ca)
        .and_then(|found| found.collect::<Result<Vec<_>, _>>())
        .map_err(|err| refuse(&err))?;
    let mut roots = RootCertStore::empty();
    for cert in certs {
        roots.add(cert).map_err(|err| refuse(&err))?;
    }
    WebPkiClientVerifier::builder_with_provider(Arc::new(roots), Arc::new(default_provider()))
        .allow_unauthenticated()
        .build()
        .map_err(|err| refuse(&err))
}

impl ClientCertVerifier for ServiceVerifier {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        false
    }

    /// No hint: the client is not told which CAs to choose from, so that a reload of the CA
    /// file needs no list kept in step with it.
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        now: UnixTime,
    ) -> Result<ClientCertVerified, Error> {
        let verified = self
            .current()
            .verify_client_cert(end_entity, intermediates, now)?;
        host_of(end_entity.as_ref()).map_err(|fault| Error::General(fault.to_string()))?;
        Ok(verified)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.current().verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.current().verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.current().supported_verify_schemes()
    }
}

#[cfg(test)]
#[path = "client_verifier_tests.rs"]
mod tests;
