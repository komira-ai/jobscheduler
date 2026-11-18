//! The server's TLS configuration, from the files of `--tls-cert` and `--tls-key`, and
//! the reload of them while the server runs.
//!
//! The certificate and key are one `CertifiedKey` behind a lock that the handshake reads
//! once, so a handshake sees the old pair or the new pair and never one of each. A
//! connection that is already open keeps what it negotiated.
//!
//! There are two acceptors over that one pair, so a reload reaches both: the lease and
//! agent listeners' speaks HTTP/1.1 only, as it always has, and the job service's offers
//! HTTP/2 (`h2`, which gRPC needs) before HTTP/1.1, the two its port serves.
//!
//! With `--agent-client-ca` the lease and agent listeners' acceptor also asks a client for a
//! certificate, which it need not give ([`crate::client_verifier`]); the job service's asks
//! for none. The CA file is read again with the pair, and a reload takes both or neither.

use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock};

use rustls::ServerConfig;
use rustls::crypto::ring::default_provider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use tokio_rustls::TlsAcceptor;
use x509_parser::parse_x509_certificate;

use js_types::Clock;

use crate::client_verifier::{self, HostCertVerifier};
use crate::clock::SharedClock;
use crate::config::{TlsFiles, read_private};
use crate::error::StartupError;

/// The pair the handshakes use, replaced as a whole.
#[derive(Debug)]
struct Current(RwLock<Arc<CertifiedKey>>);

impl ResolvesServerCert for Current {
    fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(
            self.0
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .clone(),
        )
    }
}

/// The server's TLS: the acceptors for the listeners and the means to reload the pair.
pub(crate) struct Tls {
    files: TlsFiles,
    clock: SharedClock,
    current: Arc<Current>,
    /// The CA file of `--agent-client-ca` and the verifier that trusts it, when given.
    clients: Option<(PathBuf, Arc<HostCertVerifier>)>,
    acceptor: TlsAcceptor,
    job_acceptor: TlsAcceptor,
}

impl Tls {
    /// TLS serving the certificate chain and key in `files`: HTTP/1.1 only on the lease and
    /// agent listeners, HTTP/2 or HTTP/1.1 on the job service's. The key file must be
    /// owner-only like the token file (see `read_private`). With `client_ca`, the lease and
    /// agent listeners accept the host certificates of that CA file.
    pub(crate) fn open(
        files: &TlsFiles,
        client_ca: Option<&Path>,
        clock: SharedClock,
    ) -> Result<Self, StartupError> {
        let current = Arc::new(Current(RwLock::new(Arc::new(load(files)?))));
        let clients = match client_ca {
            Some(ca) => Some((ca.to_path_buf(), Arc::new(HostCertVerifier::open(ca)?))),
            None => None,
        };
        let builder = || {
            ServerConfig::builder_with_provider(Arc::new(default_provider()))
                .with_safe_default_protocol_versions()
                .map_err(StartupError::because("--tls-cert"))
        };
        let mut config = match &clients {
            Some((_, verifier)) => builder()?.with_client_cert_verifier(verifier.clone()),
            None => builder()?.with_no_client_auth(),
        }
        .with_cert_resolver(current.clone());
        if clients.is_some() {
            // A resumed session restores the client's certificates without checking them
            // again, so an expired leaf or a removed CA would keep admitting. With a client
            // verifier every connection is a full handshake.
            config.session_storage = Arc::new(rustls::server::NoServerSessionStorage {});
            config.send_tls13_tickets = 0;
        }
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let mut jobs = builder()?
            .with_no_client_auth()
            .with_cert_resolver(current.clone());
        jobs.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        Ok(Self {
            files: files.clone(),
            clock,
            current,
            clients,
            acceptor: TlsAcceptor::from(Arc::new(config)),
            job_acceptor: TlsAcceptor::from(Arc::new(jobs)),
        })
    }

    /// The acceptor of the lease and agent listeners; it follows every reload.
    pub(crate) fn acceptor(&self) -> TlsAcceptor {
        self.acceptor.clone()
    }

    /// The acceptor of the job service's listener; it follows every reload.
    pub(crate) fn job_acceptor(&self) -> TlsAcceptor {
        self.job_acceptor.clone()
    }

    /// Reads the files again and, when the pair is whole (the key owner-only and the key
    /// of the certificate) and the leaf certificate is valid now by the server's clock, serves it from the next
    /// handshake on; with `--agent-client-ca`, its file is read again too, and the
    /// certificates in it are trusted from the next handshake on. Otherwise the pair and the
    /// CA being served stay (a CA file that fails keeps the pair too) and the error says why.
    pub(crate) fn reload(&self) -> Result<(), StartupError> {
        let fresh = load(&self.files)?;
        let now = i64::try_from(self.clock.now().millis() / 1000).unwrap_or(i64::MAX);
        check_valid_now(&fresh, &self.files, now)?;
        let roots = match &self.clients {
            Some((ca, verifier)) => Some((verifier, client_verifier::load(ca)?)),
            None => None,
        };
        roots
            .into_iter()
            .for_each(|(verifier, roots)| verifier.replace(roots));
        *self
            .current
            .0
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Arc::new(fresh);
        Ok(())
    }
}

/// The pair in `files`, with the key checked to be the certificate's.
fn load(files: &TlsFiles) -> Result<CertifiedKey, StartupError> {
    let certs = CertificateDer::pem_file_iter(&files.cert)
        .and_then(|found| found.collect::<Result<Vec<_>, _>>())
        .map_err(|err| {
            StartupError::new("--tls-cert", format!("{}: {err}", files.cert.display()))
        })?;
    if certs.is_empty() {
        let reason = format!("{}: no certificate found", files.cert.display());
        return Err(StartupError::new("--tls-cert", reason));
    }
    let key_bytes = read_private(&files.key, "--tls-key")?;
    let key = PrivateKeyDer::from_pem_slice(&key_bytes)
        .map_err(|err| StartupError::new("--tls-key", format!("{}: {err}", files.key.display())))?;
    let signer = default_provider()
        .key_provider
        .load_private_key(key)
        .map_err(StartupError::because("--tls-key"))?;
    let pair = CertifiedKey::new(certs, signer);
    pair.keys_match()
        .map_err(|err| StartupError::new("--tls-key", format!("{}: {err}", files.key.display())))?;
    Ok(pair)
}

/// Refuses a pair whose leaf certificate is not valid at `now` (seconds since the Unix epoch).
fn check_valid_now(pair: &CertifiedKey, files: &TlsFiles, now: i64) -> Result<(), StartupError> {
    let refuse =
        |why: &str| StartupError::new("--tls-cert", format!("{}: {why}", files.cert.display()));
    let unreadable = refuse("the validity of the certificate cannot be read");
    let parsed = pair
        .cert
        .first()
        .and_then(|der| parse_x509_certificate(der.as_ref()).ok());
    let (_, cert) = parsed.ok_or(unreadable)?;
    let (from, to) = (
        cert.validity().not_before.timestamp(),
        cert.validity().not_after.timestamp(),
    );
    if now < from {
        return Err(refuse("the certificate is not valid yet"));
    }
    if now > to {
        return Err(refuse("the certificate has expired"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair_with_cert(der: Vec<u8>) -> CertifiedKey {
        let key = rcgen::KeyPair::generate().unwrap();
        let der_key = PrivateKeyDer::try_from(key.serialize_der()).unwrap();
        let signer = default_provider()
            .key_provider
            .load_private_key(der_key)
            .unwrap();
        CertifiedKey::new(vec![CertificateDer::from(der)], signer)
    }

    #[test]
    fn a_pair_whose_certificate_cannot_be_read_is_refused() {
        let files = TlsFiles {
            cert: "cert.pem".into(),
            key: "key.pem".into(),
        };
        let err = check_valid_now(&pair_with_cert(vec![0x30, 0x00]), &files, 0).unwrap_err();
        assert_eq!(err.flag(), "--tls-cert");
        assert!(err.to_string().contains("cannot be read"), "{err}");
    }
}
