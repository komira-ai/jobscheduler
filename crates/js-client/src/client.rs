//! The HTTP client: one agent per command, bound to one server and one token or one
//! host identity, which keeps its connection to the server between requests, and finds the
//! server at the address its book gives (see [`crate::address`]).

use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use rustls::crypto::ring::default_provider;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;
use serde_json::Value;
use ureq::RequestBuilder;
use ureq::tls::{Certificate, ClientCert, RootCerts, TlsConfig, TlsProvider};
use ureq::unversioned::transport::{Connector, RustlsConnector, TcpConnector};
use ureq::{Agent, Error as TransportError};

use crate::address::{self, Book, Remember, Resolve};
use crate::error::ClientError;
use crate::identity::{HostIdentity, Live};
use crate::server_url::ServerUrl;
use crate::token;

/// The longest a request may take from first byte to last, connect and TLS included.
pub const REQUEST_LIMIT: Duration = Duration::from_secs(30);

/// The longest a connection is kept idle for the next request. It is above the agent's
/// heartbeat interval ([`js_agent_wire::DEFAULT_HEARTBEAT_INTERVAL_MS`], 5 s), so the beats of
/// a host ride one connection, and below the time the lease server keeps an idle connection
/// open (its `header_timeout`, 10 s by default), so the client lets a connection go before
/// the server closes it under a request sent on it.
const IDLE_LIMIT: Duration = Duration::from_secs(8);

// The beats of a host at the default interval find their connection still kept.
const _: () = assert!(
    Duration::from_millis(js_agent_wire::DEFAULT_HEARTBEAT_INTERVAL_MS).as_nanos()
        < IDLE_LIMIT.as_nanos()
);

/// The largest reply read: the wire's bound on a reply body,
/// [`js_agent_wire::limits::REPLY_BODY_BUDGET`] (1 MiB). The server's answers are small.
const REPLY_LIMIT: u64 = js_agent_wire::limits::REPLY_BODY_BUDGET as u64;

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

/// How a client proves who it is.
#[derive(Clone)]
enum Auth {
    /// The `Authorization` header value, on an agent built once.
    Bearer { agent: Agent, bearer: String },
    /// A client certificate, on the agent of the newest valid pair of files.
    Identity(Arc<Live>),
}

/// A connection to one server. It holds the token, or the key of its host identity, and
/// never prints it: it has no `Debug`.
#[derive(Clone)]
pub struct Client {
    auth: Auth,
    server: ServerUrl,
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
        let book = address::book(&server);
        let agent = build_agent(server.https, limit, IDLE_LIMIT, &roots, None, &book);
        Ok(Self {
            auth: Auth::Bearer { agent, bearer },
            server,
            limit,
        })
    }

    /// Like [`Client::connect`], but the client proves who it is by the host identity
    /// in `identity`: a client certificate presented in the TLS handshake, and no
    /// `Authorization` header. Both files are read again before each request; when their
    /// bytes changed and the pair they now hold is valid (both parse, the key passes
    /// [`crate::host_key_problem`] and goes with the certificate), later requests use it, and otherwise the client
    /// keeps the pair it had. A renewed pair is taken up without a new client.
    ///
    /// The identity is a client certificate, which exists only in a TLS handshake: a server
    /// that is plain `http://` is refused, naming `--server`, even where a token client would
    /// be let go to a loopback address over `http://`.
    ///
    /// # Errors
    ///
    /// [`ClientError::Flag`] naming `--server` or `--ca-cert` as for [`Client::connect`],
    /// or `--host-cert` or `--host-key` and the file when the identity cannot be read or is
    /// not valid.
    /// The text never holds the contents of a file.
    pub fn connect_with_identity(
        server: &str,
        identity: &HostIdentity,
        ca_cert: Option<&Path>,
        limit: Duration,
    ) -> Result<Self, ClientError> {
        let server = ServerUrl::parse(server)?;
        if !server.https {
            return Err(ClientError::flag(
                "--server",
                "the host identity is a client certificate, presented only over https://",
            ));
        }
        let roots = roots(server.https, ca_cert)?;
        let https = server.https;
        let book = address::book(&server);
        let live = Live::open(identity.clone(), move |cert| {
            build_agent(https, limit, IDLE_LIMIT, &roots, Some(cert), &book)
        })?;
        Ok(Self {
            auth: Auth::Identity(live),
            server,
            limit,
        })
    }

    /// Makes `secret` the bearer of every later request, in place of the token the client
    /// was built with. The agent calls it once it holds the credential a welcome carried.
    /// The client keeps the old bearer when `secret` is refused.
    ///
    /// # Errors
    ///
    /// [`ClientError::Flag`] when `secret` is empty or is not one run of printable ASCII
    /// (see [`bearer_of`]), and when the client authenticates by a host identity, which
    /// takes no bearer.
    pub fn set_bearer(&mut self, secret: &str) -> Result<(), ClientError> {
        match &mut self.auth {
            Auth::Bearer { bearer, .. } => {
                *bearer = token::bearer_of(secret)?;
                Ok(())
            }
            Auth::Identity(_) => Err(ClientError::flag(
                "--token-file",
                "the client authenticates by its host identity and takes no bearer",
            )),
        }
    }

    /// The agent for the next request, and the `Authorization` value to send with it (none
    /// for a host identity).
    fn route(&self) -> (Agent, Option<&str>) {
        match &self.auth {
            Auth::Bearer { agent, bearer } => (agent.clone(), Some(bearer)),
            Auth::Identity(live) => (live.agent(), None),
        }
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
        let (agent, bearer) = self.route();
        let request = authorized(agent.get(self.url(path)), bearer);
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
        let (agent, bearer) = self.route();
        let request = agent
            .post(self.url(path))
            .config()
            .timeout_global(Some(limit.min(self.limit)))
            .build();
        let request = authorized(request, bearer).content_type("application/json");
        self.reply(request.send(body.to_string()))
    }

    /// `DELETE path`.
    pub fn delete(&self, path: &str) -> Result<Reply, ClientError> {
        let (agent, bearer) = self.route();
        let request = authorized(agent.delete(self.url(path)), bearer);
        self.reply(request.call())
    }

    /// `DELETE path` with a JSON body, for a route that names what to remove in the body.
    pub fn delete_with(&self, path: &str, body: &Value) -> Result<Reply, ClientError> {
        let (agent, bearer) = self.route();
        let request = authorized(agent.delete(self.url(path)).force_send_body(), bearer)
            .content_type("application/json");
        self.reply(request.send(body.to_string()))
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

/// `request` with the `Authorization` value, when there is one.
fn authorized<B>(request: RequestBuilder<B>, bearer: Option<&str>) -> RequestBuilder<B> {
    match bearer {
        Some(value) => request.header("authorization", value),
        None => request,
    }
}

/// The HTTP agent for one server: no proxy, no redirect, at most one idle connection kept
/// for less than `idle` (the clients pass [`IDLE_LIMIT`]), and TLS with `roots` and, when
/// given, a client certificate. The clones of a [`Client`] share the agent and so the
/// connection. Its addresses are `book`'s: it asks no resolver of its own, and each new
/// connection is made by TCP then TLS (which checks the certificate against the server's
/// name, whatever the address) on one address at a time.
pub(crate) fn build_agent(
    https: bool,
    limit: Duration,
    idle: Duration,
    roots: &RootCerts,
    cert: Option<ClientCert>,
    book: &Arc<Book>,
) -> Agent {
    let tls = TlsConfig::builder()
        .provider(TlsProvider::Rustls)
        .root_certs(roots.clone())
        .client_cert(cert)
        .unversioned_rustls_crypto_provider(Arc::new(default_provider()))
        .build();
    let config = Agent::config_builder()
        .proxy(None)
        .max_redirects(0)
        .http_status_as_error(false)
        .https_only(https)
        .timeout_global(Some(limit))
        // One idle connection is kept for the next request, so a host's heartbeats ride one
        // TCP connection and one TLS handshake. It is let go after `idle`, before the
        // server closes it. One the server said it would close (`Connection: close`, as on
        // the answer to its `max_requests`th request) is not kept, and one it closed sooner
        // (a drain, a restart) is found closed when it is taken from the pool and a new one
        // is opened instead. Nothing is sent twice: a request that meets the server's close
        // on the way fails as a request to an unreachable server does.
        .max_idle_connections(1)
        .max_idle_connections_per_host(1)
        .max_idle_age(idle)
        .tls_config(tls)
        .build();
    let connect = ().chain(TcpConnector::default()).chain(RustlsConnector::default());
    let connector = Remember {
        inner: connect,
        book: Arc::clone(book),
    };
    Agent::with_parts(config, connector, Resolve(Arc::clone(book)))
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

/// The certificates in a PEM file; a file without one is refused. The job client reads
/// `--ca-cert` through this too.
pub(crate) fn pem_roots(path: &Path) -> Result<Vec<CertificateDer<'static>>, ClientError> {
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
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread::{self, JoinHandle};

    use super::*;

    /// A plain http server on 127.0.0.1 that takes `connections` connections one after the
    /// other and answers every request on each with an empty 200, keeping the connection
    /// open until the client closes it. It returns how many requests each connection
    /// carried.
    fn keep_alive(connections: usize) -> (String, JoinHandle<Vec<usize>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let served = thread::spawn(move || {
            (0..connections)
                .map(|_| {
                    let (mut tcp, _) = listener.accept().unwrap();
                    let (mut requests, mut head, mut byte) = (0, Vec::new(), [0_u8; 1]);
                    while tcp.read(&mut byte).unwrap() == 1 {
                        head.push(byte[0]);
                        if head.ends_with(b"\r\n\r\n") {
                            head.clear();
                            requests += 1;
                            tcp.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n")
                                .unwrap();
                        }
                    }
                    requests
                })
                .collect()
        });
        (url, served)
    }

    /// Two requests in a row on an agent built with `idle`; returns the requests each
    /// connection the server took carried, once the agent is gone.
    fn two_requests(idle: Duration, connections: usize) -> Vec<usize> {
        let (url, served) = keep_alive(connections);
        let agent = build_agent(
            false,
            REQUEST_LIMIT,
            idle,
            &roots(false, None).unwrap(),
            None,
            &address::book(&ServerUrl::parse(&url).unwrap()),
        );
        for path in ["/a", "/b"] {
            let reply = agent.get(format!("{url}{path}")).call().unwrap();
            assert_eq!(reply.status().as_u16(), 200);
        }
        drop(agent);
        served.join().unwrap()
    }

    /// Asserts that the idle limit is what decides whether the kept connection is used
    /// again, while the server keeps it open: with [`IDLE_LIMIT`] the second request rides
    /// the first one's connection, and with a limit of zero, which every kept connection
    /// has already reached when the next request is made (the clock without a wait), it
    /// opens a new one. It catches an agent that ignores the limit it is given and keeps a
    /// connection past it, which the server may have closed by then.
    #[test]
    fn a_connection_idle_past_the_limit_is_not_used_again() {
        assert_eq!(two_requests(IDLE_LIMIT, 1), vec![2]);
        assert_eq!(two_requests(Duration::ZERO, 2), vec![1, 1]);
    }

    /// Asserts that without `--ca-cert` an `https://` server is checked against the
    /// bundled `webpki-roots` set, not a file and not the platform's store, and that plain
    /// http names no root at all. It catches a change of the default to an empty, a
    /// system or a trust-everything store.
    #[test]
    fn no_ca_cert_means_the_bundled_public_roots() {
        // Compared as text: a `matches!` arm that never fails is a branch no test takes.
        assert_eq!(format!("{:?}", roots(true, None).unwrap()), "WebPki");
        assert_eq!(format!("{:?}", roots(false, None).unwrap()), "Specific([])");
        // What `RootCerts::WebPki` means in ureq: exactly this crate's anchors.
        let bundled = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        assert!(!bundled.is_empty());
        assert_eq!(bundled.len(), webpki_roots::TLS_SERVER_ROOTS.len());
    }
}
