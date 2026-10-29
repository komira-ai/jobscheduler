//! A small HTTP/1.1 client for a registry: one connection per request over rustls,
//! every connection to an address the policy allows, redirects followed without
//! credentials, every wait bounded.
//!
//! Written over hyper's connection API rather than a client crate: a program that links
//! this links no standalone HTTP client.

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt as _, Empty};
use hyper::body::Incoming;
use hyper::header::{AUTHORIZATION, CONNECTION, HOST, HeaderName, LOCATION, USER_AGENT};
use hyper::{Request, Response, Uri};
use hyper_util::rt::TokioIo;
use rustls::pki_types::{CertificateDer, ServerName};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

use crate::location::{Authority, HTTPS_PORT};
use crate::text;
use crate::{Error, Policy};

/// How long each kind of wait may last.
///
/// `connect`, `head` and `idle` bound one wait each: a server that answers each in time
/// but slowly can still hold a pull for as long as it likes, which is what `manifest`
/// and `blob` are for. Each is a wall-clock limit on a whole fetch, redirects and token
/// requests included, and the fetch fails with a transport error when it passes.
///
/// The defaults: `connect` 30 s, `head` 60 s, `idle` 60 s, `manifest` 5 minutes (a
/// manifest is at most 4 MiB) and `blob` 2 hours (a bundle or image is at most 16 GiB,
/// so a link that gives 2.4 MB/s still finishes). A caller with slower links raises
/// `blob`; one that wants a drip-feeding registry cut off sooner lowers it.
#[derive(Clone, Copy, Debug)]
pub struct Timeouts {
    /// Resolving and connecting, including the TLS handshake.
    pub connect: Duration,
    /// From the request to the response's headers.
    pub head: Duration,
    /// Between two chunks of a body: a stall, not a total.
    pub idle: Duration,
    /// The whole of fetching one manifest, from the first request to the last byte.
    pub manifest: Duration,
    /// The whole of fetching one blob (a config or a layer), from the first request to
    /// the sink's `finish`.
    pub blob: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(30),
            head: Duration::from_secs(60),
            idle: Duration::from_secs(60),
            manifest: Duration::from_secs(5 * 60),
            blob: Duration::from_secs(2 * 60 * 60),
        }
    }
}

/// What the client's root store held when it was built.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RootSummary {
    /// Certificates trusted.
    pub loaded: usize,
    /// Certificates (or files of them) that could not be read, and were left out.
    pub unreadable: usize,
}

/// Redirects followed for one request.
const MAX_REDIRECTS: usize = 5;

/// The roots a client trusts.
#[derive(Clone, Debug)]
pub(crate) enum Roots {
    /// The host's system store.
    System,
    /// Exactly these.
    Given(Vec<CertificateDer<'static>>),
}

pub(crate) struct Http {
    roots: Roots,
    pub(crate) policy: Policy,
    /// Host names that resolve to a given address without asking the resolver.
    resolve: BTreeMap<String, IpAddr>,
    pub(crate) timeouts: Timeouts,
    tls: OnceLock<Result<(TlsConnector, RootSummary), String>>,
}

impl std::fmt::Debug for Http {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Http")
            .field("policy", &self.policy)
            .field("resolve", &self.resolve)
            .field("timeouts", &self.timeouts)
            .finish_non_exhaustive()
    }
}

impl Http {
    pub(crate) fn new(policy: Policy, roots: Roots) -> Self {
        Self {
            roots,
            policy,
            resolve: BTreeMap::new(),
            timeouts: Timeouts::default(),
            tls: OnceLock::new(),
        }
    }

    pub(crate) fn resolve_to(&mut self, host: &str, ip: IpAddr) {
        self.resolve.insert(host.to_ascii_lowercase(), ip);
    }

    /// The TLS client and what its roots were, made on first use: loading the system
    /// store is not free, and a client that fetches nothing never does.
    fn connector(&self) -> Result<&(TlsConnector, RootSummary), Error> {
        let made = self.tls.get_or_init(|| {
            let (certs, failed) = match &self.roots {
                Roots::System => {
                    let found = rustls_native_certs::load_native_certs();
                    (found.certs, found.errors.len())
                }
                Roots::Given(certs) => (certs.clone(), 0),
            };
            let mut store = rustls::RootCertStore::empty();
            let (added, ignored) = store.add_parsable_certificates(certs);
            let summary = RootSummary {
                loaded: added,
                unreadable: ignored + failed,
            };
            if added == 0 {
                return Err(format!(
                    "no trusted root certificates ({} could not be read)",
                    summary.unreadable
                ));
            }
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let mut config = rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .expect("the ring provider supports the default TLS versions")
                .with_root_certificates(store)
                .with_no_client_auth();
            config.alpn_protocols = vec![b"http/1.1".to_vec()];
            Ok((TlsConnector::from(Arc::new(config)), summary))
        });
        made.as_ref().map_err(|why| Error::Transport {
            what: "TLS".to_owned(),
            why: why.clone(),
        })
    }

    /// The roots this client trusts, loading them if nothing has yet.
    pub(crate) fn roots(&self) -> Result<RootSummary, Error> {
        self.connector().map(|(_, summary)| *summary)
    }

    /// The addresses `host` resolves to, each checked against the policy. The caller
    /// connects to these addresses and no others, so a name that answers differently a
    /// second time cannot move the connection.
    async fn addresses(&self, host: &Authority) -> Result<Vec<SocketAddr>, Error> {
        let port = host.port();
        let found: Vec<SocketAddr> = match self.resolve.get(host.host()) {
            Some(ip) => vec![SocketAddr::new(*ip, port)],
            None => tokio::net::lookup_host((host.host(), port))
                .await
                .map_err(|e| Error::Transport {
                    what: host.to_string(),
                    why: format!("resolving: {e}"),
                })?
                .collect(),
        };
        for addr in &found {
            self.policy.address_ok(host, addr.ip())?;
        }
        Ok(found)
    }

    /// GETs `url` with `headers`, following redirects. The headers go to `url` only: a
    /// redirect target (a blob's CDN) gets the request without `Authorization`. `own`
    /// is the registry (or token endpoint) the request began at: every hop may use port
    /// 443 or `own`'s port and no other.
    pub(crate) async fn get(
        &self,
        url: &str,
        headers: &[(HeaderName, String)],
        own: &Authority,
    ) -> Result<Response<Incoming>, Error> {
        let mut url = url.to_owned();
        let mut headers = headers.to_vec();
        for hop in 0..=MAX_REDIRECTS {
            let response = self.get_once(&url, &headers, own, hop == 0).await?;
            if !response.status().is_redirection() {
                return Ok(response);
            }
            let to = response
                .headers()
                .get(LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| Error::Transport {
                    what: text::url(&url),
                    why: "a redirect with no Location".to_owned(),
                })?;
            url = resolve(&url, to)?;
            headers.retain(|(name, _)| name != AUTHORIZATION);
        }
        Err(Error::Transport {
            what: text::url(&url),
            why: format!("more than {MAX_REDIRECTS} redirects"),
        })
    }

    async fn get_once(
        &self,
        url: &str,
        headers: &[(HeaderName, String)],
        own: &Authority,
        first: bool,
    ) -> Result<Response<Incoming>, Error> {
        // What an error may say of the URL: no query, fragment or userinfo.
        let transport = |why: String| Error::Transport {
            what: text::url(url),
            why,
        };
        let uri: Uri = url.parse().map_err(|e| transport(format!("{e}")))?;
        let authority = Authority::of_uri(&uri).filter(|_| uri.scheme_str() == Some("https"));
        let Some(authority) = authority else {
            return Err(Error::Insecure(text::url(url)));
        };
        if first && ![HTTPS_PORT, own.port()].contains(&authority.port()) {
            return Err(Error::Port {
                host: authority.host().to_owned(),
                port: authority.port(),
                own: own.port(),
            });
        }
        // Before connecting: a name that cannot be a server's is no reason to dial.
        let name = ServerName::try_from(authority.host().to_owned())
            .map_err(|e| transport(e.to_string()))?;
        let connect = async {
            let addrs = self.addresses(&authority).await?;
            let tcp = TcpStream::connect(addrs.as_slice())
                .await
                .map_err(|e| transport(format!("connect: {e}")))?;
            let stream = self
                .connector()?
                .0
                .connect(name, tcp)
                .await
                .map_err(|e| transport(format!("TLS: {e}")))?;
            Ok::<_, Error>(send(TokioIo::new(stream)).await)
        };
        let mut sender = tokio::time::timeout(self.timeouts.connect, connect)
            .await
            .map_err(|_| transport("connecting timed out".to_owned()))??;
        let mut request = Request::get(uri.path_and_query().map_or("/", |p| p.as_str()))
            .header(HOST, authority.header())
            .header(USER_AGENT, "js-oci")
            .header(CONNECTION, "close");
        for (name, value) in headers {
            request = request.header(name, value);
        }
        let request = request
            .body(Empty::<Bytes>::new())
            .map_err(|e| transport(e.to_string()))?;
        tokio::time::timeout(self.timeouts.head, sender.send_request(request))
            .await
            .map_err(|_| transport("no response headers in time".to_owned()))?
            .map_err(|e| transport(e.to_string()))
    }

    /// Runs `work` for at most `limit`: a fetch that outlasts it fails with a transport
    /// error naming `what`. Each wait inside a fetch is bounded by its own timeout; this
    /// bounds the sum.
    pub(crate) async fn within<T>(
        &self,
        limit: Duration,
        what: &str,
        work: impl Future<Output = Result<T, Error>>,
    ) -> Result<T, Error> {
        match tokio::time::timeout(limit, work).await {
            Ok(done) => done,
            Err(_) => Err(Error::Transport {
                what: text::url(what),
                why: "did not finish within its time limit".to_owned(),
            }),
        }
    }

    /// The next chunk of `body`, or `None` at its end.
    pub(crate) async fn chunk(
        &self,
        what: &str,
        body: &mut Incoming,
    ) -> Result<Option<Bytes>, Error> {
        let frame = tokio::time::timeout(self.timeouts.idle, body.frame())
            .await
            .map_err(|_| Error::Transport {
                what: text::url(what),
                why: "the body stalled".to_owned(),
            })?;
        match frame {
            None => Ok(None),
            Some(Err(e)) => Err(Error::Transport {
                what: text::url(what),
                why: e.to_string(),
            }),
            // Trailers come after all the data: a trailers frame is the body's end.
            Some(Ok(frame)) => Ok(frame.into_data().ok()),
        }
    }

    /// The whole body of `response`, at most `limit` bytes.
    pub(crate) async fn read_limited(
        &self,
        what: &str,
        response: Response<Incoming>,
        limit: u64,
    ) -> Result<Vec<u8>, Error> {
        let mut body = response.into_body();
        let mut out = Vec::new();
        while let Some(data) = self.chunk(what, &mut body).await? {
            if out.len() as u64 + data.len() as u64 > limit {
                return Err(Error::TooLarge {
                    what: text::url(what),
                    limit,
                });
            }
            out.extend_from_slice(&data);
        }
        Ok(out)
    }
}

/// Runs the HTTP/1 handshake over `io`, the connection's driver on a task of its own.
async fn send<I>(io: TokioIo<I>) -> hyper::client::conn::http1::SendRequest<Empty<Bytes>>
where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    // The handshake does no I/O and has no failure to report; what a broken connection
    // says comes out of the request, and of the body.
    let (sender, connection) = hyper::client::conn::http1::handshake(io)
        .await
        .expect("hyper's HTTP/1 handshake does no I/O");
    tokio::spawn(async move {
        // It ends with the response; an error then is the server closing early, which
        // the body read reports.
        let _ = connection.await;
    });
    sender
}

/// The URL `location` names when it is a redirect from `from`, as RFC 3986 section 5
/// resolves a reference: absolute, a network-path reference (`//host/path`), an
/// absolute path, a relative path, or a bare query. The fragment is dropped (it is not
/// sent), and dot segments are removed. Whether the result is https and where it points
/// is for the caller's checks.
fn resolve(from: &str, location: &str) -> Result<String, Error> {
    let location = location.split('#').next().unwrap_or_default();
    let uri: Uri = from.parse().map_err(|e| Error::Transport {
        what: text::url(from),
        why: format!("{e}"),
    })?;
    let (Some(scheme), Some(authority)) = (uri.scheme_str(), uri.authority()) else {
        return Err(Error::Transport {
            what: text::url(from),
            why: "not an absolute URL".to_owned(),
        });
    };
    if has_scheme(location) {
        return Ok(location.to_owned());
    }
    if location.starts_with("//") {
        return Ok(format!("{scheme}:{location}"));
    }
    let (path, query) = match location.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (location, None),
    };
    let merged = if path.is_empty() {
        uri.path().to_owned()
    } else if path.starts_with('/') {
        remove_dot_segments(path)
    } else {
        // Relative to the directory of the request's path.
        let base = uri.path();
        let dir = &base[..=base.rfind('/').unwrap_or(0)];
        remove_dot_segments(&format!("{dir}{path}"))
    };
    let query = match (query, path.is_empty()) {
        (Some(query), _) => Some(query),
        (None, true) => uri.query(),
        (None, false) => None,
    };
    Ok(match query {
        Some(query) => format!("{scheme}://{authority}{merged}?{query}"),
        None => format!("{scheme}://{authority}{merged}"),
    })
}

/// Whether `reference` begins with a scheme (`https:`): a letter, then letters, digits,
/// `+`, `-` and `.`, then a colon. A colon after a `/`, `?` or `#` is not one.
fn has_scheme(reference: &str) -> bool {
    let Some((scheme, _)) = reference.split_once(':') else {
        return false;
    };
    let mut bytes = scheme.bytes();
    bytes.next().is_some_and(|b| b.is_ascii_alphabetic()) && bytes.all(text::scheme_byte)
}

/// `path` (which begins with `/`) with its `.` and `..` segments removed (RFC 3986
/// section 5.2.4).
fn remove_dot_segments(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').skip(1).collect();
    let last = parts.len() - 1;
    let mut out: Vec<&str> = Vec::new();
    for (at, part) in parts.into_iter().enumerate() {
        match part {
            "." => {}
            ".." => {
                out.pop();
            }
            other => out.push(other),
        }
        // A path that ends in a dot segment ends in a slash.
        if at == last && matches!(part, "." | "..") {
            out.push("");
        }
    }
    format!("/{}", out.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a redirect target built wrongly: absolute, rooted and relative forms,
    /// and a base that is no URL.
    #[test]
    fn locations_resolve_against_the_request() {
        let from = "https://r.example:5000/v2/a/blobs/sha256:x";
        assert_eq!(
            resolve(from, "https://cdn.example/blob?sig=1").unwrap(),
            "https://cdn.example/blob?sig=1"
        );
        assert_eq!(
            resolve(from, "/other?x=1").unwrap(),
            "https://r.example:5000/other?x=1"
        );
        assert_eq!(
            resolve(from, "sibling").unwrap(),
            "https://r.example:5000/v2/a/blobs/sibling"
        );
        assert!(resolve("not a url", "/x").is_err());
        assert!(resolve("/relative", "/x").is_err());
    }

    /// Catches a `Location` resolved by string tricks instead of RFC 3986: a `://` inside
    /// a query is still a path, `//host/path` is a network-path reference, `?query` keeps
    /// the base path, fragments are dropped, and dot segments are removed.
    #[test]
    fn locations_resolve_as_uri_references() {
        let from = "https://r.example:5000/v2/a/b/blobs/sha256:x?old=1";
        for (location, want) in [
            ("/r?u=https://x", "https://r.example:5000/r?u=https://x"),
            ("//cdn.test/x?y=1", "https://cdn.test/x?y=1"),
            ("?q=2", "https://r.example:5000/v2/a/b/blobs/sha256:x?q=2"),
            ("", "https://r.example:5000/v2/a/b/blobs/sha256:x?old=1"),
            ("../c", "https://r.example:5000/v2/a/b/c"),
            ("./c", "https://r.example:5000/v2/a/b/blobs/c"),
            ("../../../../../c", "https://r.example:5000/c"),
            ("/a/./b/../c", "https://r.example:5000/a/c"),
            ("/a/b/..", "https://r.example:5000/a/"),
            ("/a/.", "https://r.example:5000/a/"),
            ("/x#frag", "https://r.example:5000/x"),
            ("https://cdn.test/y#frag", "https://cdn.test/y"),
            ("HTTP://cdn.test/z", "HTTP://cdn.test/z"),
            ("a+b-c.d:rest", "a+b-c.d:rest"),
            (":x", "https://r.example:5000/v2/a/b/blobs/:x"),
            ("1a:x", "https://r.example:5000/v2/a/b/blobs/1a:x"),
            ("a b:x", "https://r.example:5000/v2/a/b/blobs/a b:x"),
        ] {
            assert_eq!(resolve(from, location).unwrap(), want, "{location:?}");
        }
    }
}
