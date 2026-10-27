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

use crate::failure::Failure;
use crate::server_url::ServerUrl;
use crate::token;

/// The longest a request may take from first byte to last, connect and TLS included.
const REQUEST_LIMIT: Duration = Duration::from_secs(30);

/// The largest reply read; the server's answers are small.
const REPLY_LIMIT: u64 = 1024 * 1024;

/// What the server answered: a status and the JSON body (`null` when the body was empty
/// or not JSON).
#[derive(Debug)]
pub(crate) struct Reply {
    pub(crate) status: u16,
    pub(crate) body: Value,
}

/// A connection to one lease server. It holds the token and never prints it: it has no
/// `Debug`.
pub(crate) struct Client {
    agent: Agent,
    server: ServerUrl,
    bearer: String,
}

impl Client {
    /// Checks the flags and builds the client: the URL, the owner-only token file and,
    /// for `https://`, the roots to trust (the PEM file of `--ca-cert`, else the
    /// platform's). The agent reads no proxy setting and follows no redirect, so the
    /// token goes to the one host named and nowhere else.
    pub(crate) fn connect(
        server: &str,
        token_file: &Path,
        ca_cert: Option<&Path>,
    ) -> Result<Self, Failure> {
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
            .timeout_global(Some(REQUEST_LIMIT))
            .tls_config(tls)
            .build()
            .into();
        Ok(Self {
            agent,
            server,
            bearer,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.server.base)
    }

    /// `GET path`.
    pub(crate) fn get(&self, path: &str) -> Result<Reply, Failure> {
        let request = self
            .agent
            .get(self.url(path))
            .header("authorization", &self.bearer);
        self.reply(request.call())
    }

    /// `POST path` with a JSON body.
    pub(crate) fn post(&self, path: &str, body: &Value) -> Result<Reply, Failure> {
        let request = self
            .agent
            .post(self.url(path))
            .header("authorization", &self.bearer)
            .content_type("application/json");
        self.reply(request.send(body.to_string()))
    }

    /// `DELETE path`.
    pub(crate) fn delete(&self, path: &str) -> Result<Reply, Failure> {
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
    ) -> Result<Reply, Failure> {
        let unreachable = |err: TransportError| {
            Failure::Server(format!("cannot talk to {}: {err}", self.server.base))
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

/// The certificates to trust: none for plain http, the PEM file's for `--ca-cert`,
/// otherwise the platform's.
fn roots(https: bool, ca_cert: Option<&Path>) -> Result<Vec<Certificate<'static>>, Failure> {
    let der = match (https, ca_cert) {
        (false, _) => Vec::new(),
        (true, Some(path)) => pem_roots(path)?,
        (true, None) => rustls_native_certs::load_native_certs().certs,
    };
    Ok(der
        .iter()
        .map(|cert| Certificate::from_der(cert.as_ref()).to_owned())
        .collect())
}

/// The certificates in a PEM file; a file without one is refused.
fn pem_roots(path: &Path) -> Result<Vec<CertificateDer<'static>>, Failure> {
    let refuse = |why: String| Failure::flag("--ca-cert", format!("{}: {why}", path.display()));
    let found = CertificateDer::pem_file_iter(path)
        .and_then(|found| found.collect::<Result<Vec<_>, _>>())
        .map_err(|err| refuse(err.to_string()))?;
    match found.is_empty() {
        true => Err(refuse("no certificate found".to_owned())),
        false => Ok(found),
    }
}
