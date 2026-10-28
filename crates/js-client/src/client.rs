//! The HTTP client: one agent per command, bound to one server and one token.

use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use rustls::crypto::ring::default_provider;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;
use serde_json::Value;
use ureq::tls::{Certificate, RootCerts, TlsConfig, TlsProvider};
use ureq::{Agent, Error as TransportError};

use crate::error::ClientError;
use crate::server_url::ServerUrl;
use crate::token;

/// The longest a request may take from first byte to last, connect and TLS included.
pub const REQUEST_LIMIT: Duration = Duration::from_secs(30);

/// The largest reply read; the server's answers are small.
const REPLY_LIMIT: u64 = 1024 * 1024;

/// What the server answered: a status and the JSON body (`null` when the body was empty
/// or not JSON). Its `Debug` text leaves the body out: a welcome holds a session secret.
pub struct Reply {
    /// The HTTP status.
    pub status: u16,
    /// The body as JSON, or `null`.
    pub body: Value,
    /// The wait a `Retry-After` header asked for, when it holds whole seconds. The
    /// date form of the header reads as absent.
    pub retry_after: Option<Duration>,
}

impl fmt::Debug for Reply {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.debug_struct("Reply")
            .field("status", &self.status)
            .field("body", &format_args!("<redacted>"))
            .field("retry_after", &self.retry_after)
            .finish()
    }
}

/// A connection to one lease server. It holds the token and never prints it: it has no
/// `Debug`.
#[derive(Clone)]
pub struct Client {
    agent: Agent,
    server: ServerUrl,
    bearer: String,
    limit: Duration,
}

impl Client {
    /// Checks the flags and builds the client: the URL, the owner-only token file and,
    /// for `https://`, the roots to trust: the PEM file of `--ca-cert` when given (and only
    /// that file), otherwise the bundled public roots (never the platform's). The agent reads no proxy setting and follows no
    /// redirect, so the token goes to the one host named and nowhere else.
    ///
    /// # Errors
    ///
    /// [`ClientError::Flag`] naming `--server`, `--token-file` or `--ca-cert` when that
    /// flag, or the file it names, is unusable.
    pub fn connect(
        server: &str,
        token_file: &Path,
        ca_cert: Option<&Path>,
        limit: Duration,
    ) -> Result<Self, ClientError> {
        let server = ServerUrl::parse(server)?;
        let bearer = token::bearer(token_file)?;
        let roots = roots(server.https, ca_cert)?;
        let tls = TlsConfig::builder()
            .provider(TlsProvider::Rustls)
            .root_certs(roots)
            .unversioned_rustls_crypto_provider(Arc::new(default_provider()))
            .build();
        let agent: Agent = Agent::config_builder()
            .proxy(None)
            .max_redirects(0)
            .http_status_as_error(false)
            .https_only(server.https)
            .timeout_global(Some(limit))
            // A connection that sat idle between two requests (a renewal is minutes
            // apart) may have been closed by the other side, and a request sent on a dead
            // one fails; every request opens its own.
            .max_idle_connections(0)
            .tls_config(tls)
            .build()
            .into();
        Ok(Self {
            agent,
            server,
            bearer,
            limit,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.server.base)
    }

    /// `GET path`.
    ///
    /// # Errors
    ///
    /// [`ClientError::Server`] when the server cannot be reached or its answer cannot be
    /// read; the same for [`Client::post`], [`Client::post_within`] and
    /// [`Client::delete`].
    pub fn get(&self, path: &str) -> Result<Reply, ClientError> {
        let request = self
            .agent
            .get(self.url(path))
            .header("authorization", &self.bearer);
        self.reply(request.call())
    }

    /// `POST path` with a JSON body.
    pub fn post(&self, path: &str, body: &Value) -> Result<Reply, ClientError> {
        self.post_within(path, body, self.limit)
    }

    /// `POST path` with a JSON body that must be answered within `limit` (or the client's
    /// own limit, if that is shorter).
    pub fn post_within(
        &self,
        path: &str,
        body: &Value,
        limit: Duration,
    ) -> Result<Reply, ClientError> {
        let request = self
            .agent
            .post(self.url(path))
            .config()
            .timeout_global(Some(limit.min(self.limit)))
            .build()
            .header("authorization", &self.bearer)
            .content_type("application/json");
        self.reply(request.send(body.to_string()))
    }

    /// `DELETE path`.
    pub fn delete(&self, path: &str) -> Result<Reply, ClientError> {
        let request = self
            .agent
            .delete(self.url(path))
            .header("authorization", &self.bearer);
        self.reply(request.call())
    }

    /// Reads a reply to the end. A transport error and a body that cannot be read are
    /// the same failure: the server could not be reached or went away.
    fn reply(
        &self,
        sent: Result<ureq::http::Response<ureq::Body>, TransportError>,
    ) -> Result<Reply, ClientError> {
        let unreachable = |err: TransportError| {
            ClientError::Server(format!("cannot talk to {}: {err}", self.server.base))
        };
        let mut response = sent.map_err(unreachable)?;
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .and_then(|text| text.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        let read = response
            .body_mut()
            .with_config()
            .limit(REPLY_LIMIT)
            .read_to_vec();
        let bytes = read.map_err(unreachable)?;
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        Ok(Reply {
            status,
            body,
            retry_after,
        })
    }
}

/// The roots to trust: none for plain http; for `https://`, the certificates of the
/// `--ca-cert` PEM file when it is given and only those, and otherwise the public roots
/// bundled in the `webpki-roots` crate (a Mozilla set, pinned by `Cargo.lock`). The
/// platform's own store is never read: the loaders find it through environment variables
/// (`SSL_CERT_FILE`, `SSL_CERT_DIR`), so what is trusted would depend on the process's
/// surroundings.
fn roots(https: bool, ca_cert: Option<&Path>) -> Result<RootCerts, ClientError> {
    let der = match (https, ca_cert) {
        (false, _) => Vec::new(),
        (true, Some(path)) => pem_roots(path)?,
        (true, None) => return Ok(RootCerts::WebPki),
    };
    let certs = der
        .iter()
        .map(|cert| Certificate::from_der(cert.as_ref()).to_owned())
        .collect();
    Ok(RootCerts::Specific(Arc::new(certs)))
}

/// The certificates in a PEM file; a file without one is refused.
fn pem_roots(path: &Path) -> Result<Vec<CertificateDer<'static>>, ClientError> {
    let refuse = |why: String| ClientError::flag("--ca-cert", format!("{}: {why}", path.display()));
    let found = CertificateDer::pem_file_iter(path)
        .and_then(|found| found.collect::<Result<Vec<_>, _>>())
        .map_err(|err| refuse(err.to_string()))?;
    match found.is_empty() {
        true => Err(refuse("no certificate found".to_owned())),
        false => Ok(found),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts that without `--ca-cert` an `https://` server is checked against the
    /// bundled `webpki-roots` set, not a file and not the platform's store, and that plain
    /// http names no root at all. It catches a change of the default to an empty, a
    /// system or a trust-everything store.
    #[test]
    fn no_ca_cert_means_the_bundled_public_roots() {
        assert!(matches!(roots(true, None), Ok(RootCerts::WebPki)));
        let plain = roots(false, None);
        assert!(matches!(plain, Ok(RootCerts::Specific(none)) if none.is_empty()));
        // What `RootCerts::WebPki` means in ureq: exactly this crate's anchors.
        let bundled = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        assert!(!bundled.is_empty());
        assert_eq!(bundled.len(), webpki_roots::TLS_SERVER_ROOTS.len());
    }
}
