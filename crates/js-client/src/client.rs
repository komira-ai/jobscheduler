//! The HTTP client: one agent per command, bound to one server and one token.

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
/// or not JSON).
#[derive(Debug)]
pub struct Reply {
    /// The HTTP status.
    pub status: u16,
    /// The body as JSON, or `null`.
    pub body: Value,
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
    /// for `https://`, the roots to trust (the PEM file of `--ca-cert`, which is required:
    /// the platform's roots are not read). The agent reads no proxy setting and follows no
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
            .root_certs(RootCerts::Specific(Arc::new(roots)))
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
        let read = response
            .body_mut()
            .with_config()
            .limit(REPLY_LIMIT)
            .read_to_vec();
        let bytes = read.map_err(unreachable)?;
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        Ok(Reply { status, body })
    }
}

/// The certificates to trust: none for plain http, the PEM file's for `--ca-cert`. An
/// `https://` server without `--ca-cert` is refused: the platform's roots are found
/// through the environment (the loaders honour `SSL_CERT_FILE` and `SSL_CERT_DIR`), and
/// `js` sends its token only to a server trusted by a flag.
fn roots(https: bool, ca_cert: Option<&Path>) -> Result<Vec<Certificate<'static>>, ClientError> {
    let der = match (https, ca_cert) {
        (false, _) => Vec::new(),
        (true, Some(path)) => pem_roots(path)?,
        (true, None) => {
            return Err(ClientError::flag(
                "--ca-cert",
                "required for an https:// server",
            ));
        }
    };
    Ok(der
        .iter()
        .map(|cert| Certificate::from_der(cert.as_ref()).to_owned())
        .collect())
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
