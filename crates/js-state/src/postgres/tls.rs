//! TLS for the store's connections: rustls (with ring) behind the client's connector traits,
//! one mode per `sslmode` value libpq knows, and a typed error for a handshake that fails.
//!
//! - **`disable`**: plain connections only. It is the mode when the URL names none, as it
//!   was before the client spoke TLS.
//! - **`prefer`**: TLS when the server offers it, plain when it answers no. The certificate is
//!   not checked. Unlike libpq, a handshake that fails is not tried again plain.
//! - **`require`**: TLS or no connection. The certificate is not checked, so it keeps out a
//!   listener on the wire and not a server that pretends to be the store's.
//! - **`verify-ca`**: as `require`, and the certificate chains to the root certificate the
//!   configuration names. The name in it is not checked.
//! - **`verify-full`**: as `verify-ca`, and the certificate names the host connected to.
//!
//! The two `verify` modes need a root certificate (`sslrootcert`); the platform's store is
//! never read. Postgres never offers TLS on a Unix socket, so over one `prefer` connects plain
//! and the modes that require TLS fail as [`TlsError::NotOffered`]. The client sends no
//! certificate of its own, and offers no channel binding.

use std::future::Future;
use std::io;
use std::net::Ipv4Addr;
use std::path::Path;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};

use postgres::Socket;
use postgres::config::SslMode;
use postgres::tls::{ChannelBinding, MakeTlsConnect, TlsConnect, TlsStream};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::{verify_server_cert_signed_by_trust_anchor, verify_server_name};
use rustls::crypto::{
    WebPkiSupportedAlgorithms, ring, verify_tls12_signature, verify_tls13_signature,
};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::ParsedCertificate;
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme,
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream as RustlsStream;

use super::ConfigError;

/// How a store's connections use TLS, by the `sslmode` value libpq gives each.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TlsMode {
    /// `disable`: plain connections only. The mode when the URL names none.
    #[default]
    Disable,
    /// `prefer`: TLS when the server offers it, plain when it does not; the certificate is
    /// not checked.
    Prefer,
    /// `require`: TLS or no connection; the certificate is not checked.
    Require,
    /// `verify-ca`: TLS, with a certificate that chains to the root certificate; the name in
    /// it is not checked.
    VerifyCa,
    /// `verify-full`: TLS, with a certificate that chains to the root certificate and names
    /// the host connected to.
    VerifyFull,
}

impl FromStr for TlsMode {
    type Err = ConfigError;

    /// Reads an `sslmode` value as libpq spells it. `allow` (plain first, TLS if the server
    /// insists) is not one of them: the client cannot fall back that way.
    fn from_str(name: &str) -> Result<TlsMode, ConfigError> {
        match name {
            "disable" => Ok(TlsMode::Disable),
            "prefer" => Ok(TlsMode::Prefer),
            "require" => Ok(TlsMode::Require),
            "verify-ca" => Ok(TlsMode::VerifyCa),
            "verify-full" => Ok(TlsMode::VerifyFull),
            _ => Err(ConfigError::Invalid(
                "invalid value for option `sslmode`".to_owned(),
            )),
        }
    }
}

impl TlsMode {
    /// What the client is told: it sends no TLS request, asks and falls back, or asks and
    /// gives up without TLS.
    pub(super) fn ssl_mode(self) -> SslMode {
        match self {
            TlsMode::Disable => SslMode::Disable,
            TlsMode::Prefer => SslMode::Prefer,
            TlsMode::Require | TlsMode::VerifyCa | TlsMode::VerifyFull => SslMode::Require,
        }
    }

    /// Whether a connection without TLS is refused.
    pub(super) fn requires_tls(self) -> bool {
        self.ssl_mode() == SslMode::Require
    }

    fn checks_chain(self) -> bool {
        matches!(self, TlsMode::VerifyCa | TlsMode::VerifyFull)
    }
}

/// Why a connection's TLS failed. No variant carries the URL, a host or a password.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TlsError {
    /// The server answered the TLS request with "no", and the mode does not connect plain.
    #[error("the server does not offer TLS, and the state store's sslmode requires it")]
    NotOffered,
    /// The certificate chains to the root certificate and does not name the host
    /// (`verify-full`).
    #[error("the server's certificate does not name the host the state store connects to")]
    NameMismatch,
    /// The certificate does not chain to the root certificate (`verify-ca`, `verify-full`):
    /// its issuer is unknown, or claims the root's name and is not signed by its key.
    #[error("the server's certificate is not signed by the state store's root certificate")]
    Untrusted,
    /// Any other failure of the handshake, in rustls's words.
    #[error("the TLS handshake with the server failed: {0}")]
    Handshake(String),
}

impl TlsError {
    fn of(err: &rustls::Error) -> TlsError {
        match err {
            rustls::Error::InvalidCertificate(
                CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. },
            ) => TlsError::NameMismatch,
            rustls::Error::InvalidCertificate(
                CertificateError::UnknownIssuer | CertificateError::BadSignature,
            ) => TlsError::Untrusted,
            other => TlsError::Handshake(other.to_string()),
        }
    }
}

/// The TLS half of a configuration: the mode and the rustls settings built for it once.
#[derive(Clone)]
pub(super) struct Tls {
    mode: TlsMode,
    client: Arc<ClientConfig>,
}

impl Tls {
    /// The settings for `mode`, trusting the certificates of the PEM file `root` when one is
    /// given.
    ///
    /// # Errors
    /// [`ConfigError::NoRootCert`] for a `verify` mode without `root`, and
    /// [`ConfigError::RootCert`] when the file cannot be read or holds no certificate.
    pub(super) fn new(mode: TlsMode, root: Option<&Path>) -> Result<Tls, ConfigError> {
        // Only a mode that checks the chain reads the file; the others never look at it.
        let roots = root
            .filter(|_| mode.checks_chain())
            .map(roots)
            .transpose()?;
        if mode.checks_chain() && roots.is_none() {
            return Err(ConfigError::NoRootCert);
        }
        let provider = Arc::new(ring::default_provider());
        let verifier = Verifier {
            roots: roots.map(Arc::new),
            names: mode == TlsMode::VerifyFull,
            algorithms: provider.signature_verification_algorithms,
        };
        // The provider is ring's, which has every safe protocol version, so this does not fail.
        let client = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .ok()
            .ok_or(ConfigError::Invalid("no TLS version to speak".to_owned()))?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(verifier))
            .with_no_client_auth();
        Ok(Tls {
            mode,
            client: Arc::new(client),
        })
    }

    pub(super) fn mode(&self) -> TlsMode {
        self.mode
    }

    /// The connector for one connect. `started` is set once a handshake begins, which is how
    /// a failure is told to be the server's "no" to TLS ([`Tls::refusal`]).
    pub(super) fn connector(&self, started: &Arc<AtomicBool>) -> Connector {
        Connector {
            client: Arc::clone(&self.client),
            started: Arc::clone(started),
        }
    }

    /// A failed connect, as a TLS error when it is one: a handshake rustls refused, or a mode
    /// that requires TLS meeting a server that said no (no handshake began, and the failure is
    /// not the network's). Anything else is the client's error as it came.
    pub(super) fn refusal(
        &self,
        err: postgres::Error,
        started: bool,
    ) -> Result<TlsError, postgres::Error> {
        let io = super::conn::io_cause(&err);
        if let Some(tls) = io
            .and_then(io::Error::get_ref)
            .and_then(|inner| inner.downcast_ref::<rustls::Error>())
        {
            return Ok(TlsError::of(tls));
        }
        // `&`, not `&&`: each test is asked of every failure.
        if self.mode.requires_tls() & !started & io.is_none() {
            return Ok(TlsError::NotOffered);
        }
        Err(err)
    }
}

/// The root certificates of the PEM file at `path`: every certificate in it, and at least one.
fn roots(path: &Path) -> Result<RootCertStore, ConfigError> {
    let refuse = |why: String| ConfigError::RootCert(format!("{}: {why}", path.display()));
    let certs = CertificateDer::pem_file_iter(path)
        .and_then(Iterator::collect::<Result<Vec<_>, _>>)
        .map_err(|err| refuse(err.to_string()))?;
    let mut store = RootCertStore::empty();
    let (added, _) = store.add_parsable_certificates(certs);
    if added == 0 {
        return Err(refuse("holds no certificate".to_owned()));
    }
    Ok(store)
}

/// The check of the server's certificate: none (`roots` is `None`), the chain, or the chain
/// and the name. The handshake's signatures are checked in every mode.
#[derive(Debug)]
struct Verifier {
    roots: Option<Arc<RootCertStore>>,
    names: bool,
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for Verifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let Some(roots) = &self.roots else {
            return Ok(ServerCertVerified::assertion());
        };
        let cert = ParsedCertificate::try_from(end_entity)?;
        verify_server_cert_signed_by_trust_anchor(
            &cert,
            roots,
            intermediates,
            now,
            self.algorithms.all,
        )?;
        if self.names {
            verify_server_name(&cert, server_name)?;
        }
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

/// The client's TLS connector for one connect.
pub(super) struct Connector {
    client: Arc<ClientConfig>,
    started: Arc<AtomicBool>,
}

impl MakeTlsConnect<Socket> for Connector {
    type Stream = Stream;
    type TlsConnect = Handshake;
    type Error = io::Error;

    fn make_tls_connect(&mut self, domain: &str) -> Result<Handshake, io::Error> {
        // The client asks for every host, a Unix socket's included (with an empty name), and
        // uses the answer only for a handshake, which a socket never gets (the server answers
        // no). A name rustls cannot take stands in as the unspecified address, which no
        // certificate names, so `verify-full` refuses it.
        let name = ServerName::try_from(domain.to_owned())
            .unwrap_or(ServerName::IpAddress(Ipv4Addr::UNSPECIFIED.into()));
        Ok(Handshake {
            connector: TlsConnector::from(Arc::clone(&self.client)),
            name,
            started: Arc::clone(&self.started),
        })
    }
}

/// The handshake with one host.
pub(super) struct Handshake {
    connector: TlsConnector,
    name: ServerName<'static>,
    started: Arc<AtomicBool>,
}

impl TlsConnect<Socket> for Handshake {
    type Stream = Stream;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = io::Result<Stream>> + Send>>;

    fn connect(self, socket: Socket) -> Self::Future {
        self.started.store(true, Ordering::Relaxed);
        Box::pin(async move { self.connector.connect(self.name, socket).await.map(Stream) })
    }
}

/// A connection over TLS.
pub(super) struct Stream(RustlsStream<Socket>);

impl TlsStream for Stream {
    fn channel_binding(&self) -> ChannelBinding {
        ChannelBinding::none()
    }
}

impl AsyncRead for Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_read(cx, buf)
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().0).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_shutdown(cx)
    }
}
