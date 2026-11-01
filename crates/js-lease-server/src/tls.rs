//! The server's TLS configuration, from the files of `--tls-cert` and `--tls-key`, and
//! the reload of them while the server runs.
//!
//! The certificate and key are one `CertifiedKey` behind a lock that the handshake reads
//! once, so a handshake sees the old pair or the new pair and never one of each. A
//! connection that is already open keeps what it negotiated.

use std::sync::{Arc, PoisonError, RwLock};

use rustls::ServerConfig;
use rustls::crypto::ring::default_provider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use tokio_rustls::TlsAcceptor;
use x509_parser::parse_x509_certificate;

use js_lease::Clock;

use crate::adapt::SharedClock;
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

/// The server's TLS: the acceptor for the listeners and the means to reload the pair.
pub(crate) struct Tls {
    files: TlsFiles,
    clock: SharedClock,
    current: Arc<Current>,
    acceptor: TlsAcceptor,
}

impl Tls {
    /// TLS serving the certificate chain and key in `files`, speaking HTTP/1.1 only. The
    /// key file must be owner-only like the token file (see `read_private`).
    pub(crate) fn open(files: &TlsFiles, clock: SharedClock) -> Result<Self, StartupError> {
        let current = Arc::new(Current(RwLock::new(Arc::new(load(files)?))));
        let builder = ServerConfig::builder_with_provider(Arc::new(default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(StartupError::because("--tls-cert"))?;
        let mut config = builder
            .with_no_client_auth()
            .with_cert_resolver(current.clone());
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(Self {
            files: files.clone(),
            clock,
            current,
            acceptor: TlsAcceptor::from(Arc::new(config)),
        })
    }

    /// The acceptor of the listeners; it follows every reload.
    pub(crate) fn acceptor(&self) -> TlsAcceptor {
        self.acceptor.clone()
    }

    /// Reads the files again and, when the pair is whole (the key owner-only and the key
    /// of the certificate) and the leaf certificate is valid now by the server's clock, serves it from the next
    /// handshake on. Otherwise the pair being served stays and the error says why.
    pub(crate) fn reload(&self) -> Result<(), StartupError> {
        let fresh = load(&self.files)?;
        let now = i64::try_from(self.clock.now().millis() / 1000).unwrap_or(i64::MAX);
        check_valid_now(&fresh, &self.files, now)?;
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
