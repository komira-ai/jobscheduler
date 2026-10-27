//! The server's TLS configuration, from the files of `--tls-cert` and `--tls-key`.

use std::sync::Arc;

use rustls::ServerConfig;
use rustls::crypto::ring::default_provider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::TlsAcceptor;

use crate::config::TlsFiles;
use crate::error::StartupError;

/// An acceptor serving the certificate chain and key in `files`, speaking HTTP/1.1 only.
pub(crate) fn acceptor(files: &TlsFiles) -> Result<TlsAcceptor, StartupError> {
    let certs = CertificateDer::pem_file_iter(&files.cert)
        .and_then(|found| found.collect::<Result<Vec<_>, _>>())
        .map_err(|err| {
            StartupError::new("--tls-cert", format!("{}: {err}", files.cert.display()))
        })?;
    if certs.is_empty() {
        let reason = format!("{}: no certificate found", files.cert.display());
        return Err(StartupError::new("--tls-cert", reason));
    }
    let key = PrivateKeyDer::from_pem_file(&files.key)
        .map_err(|err| StartupError::new("--tls-key", format!("{}: {err}", files.key.display())))?;
    let provider = Arc::new(default_provider());
    let builder = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(StartupError::because("--tls-cert"))?;
    let mut config = builder
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(StartupError::because("--tls-key"))?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(TlsAcceptor::from(Arc::new(config)))
}
