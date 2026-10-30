//! A registry in this process: HTTP/1.1 over TLS on a loopback socket, serving the
//! manifests and blobs a test publishes, with the behaviours the client must survive: a
//! token challenge, redirects, tampered bytes, stalls.

#![allow(dead_code)]

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use js_oci::{Allow, Client, Digest, Location, Policy};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

pub const REPO: &str = "team/bundle";
pub const LOOPBACK: IpAddr = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));

/// The names the fixture's certificate is good for.
pub const REGISTRY: &str = "registry.test";
pub const OTHER: &str = "other.test";

/// A published artifact: manifest bytes and every blob it names.
pub struct Artifact {
    pub manifest: Vec<u8>,
    pub digest: Digest,
    pub config: Vec<u8>,
    pub layers: Vec<Vec<u8>>,
}

pub const BUNDLE_CONFIG: &str = r#"{"os":"darwin","architecture":"arm64","name":"toolchain"}"#;

impl Artifact {
    /// A darwin/arm64 bundle of `layers`.
    pub fn bundle(layers: Vec<Vec<u8>>) -> Self {
        Self::bundle_with_config(layers, BUNDLE_CONFIG)
    }

    pub fn bundle_with_config(layers: Vec<Vec<u8>>, config: &str) -> Self {
        Self::build(
            "application/vnd.oci.image.manifest.v1+json",
            Some("application/vnd.komira.js.bundle.v1"),
            "application/vnd.komira.js.bundle.config.v1+json",
            config.as_bytes().to_vec(),
            "application/vnd.komira.js.bundle.layer.v1.tar+zstd",
            layers,
        )
    }

    /// A container image of `layers` with `config`.
    pub fn image(layers: Vec<Vec<u8>>, config: &str) -> Self {
        Self::build(
            "application/vnd.oci.image.manifest.v1+json",
            None,
            "application/vnd.oci.image.config.v1+json",
            config.as_bytes().to_vec(),
            "application/vnd.oci.image.layer.v1.tar+gzip",
            layers,
        )
    }

    pub fn build(
        manifest_type: &str,
        artifact_type: Option<&str>,
        config_type: &str,
        config: Vec<u8>,
        layer_type: &str,
        layers: Vec<Vec<u8>>,
    ) -> Self {
        let descriptors: Vec<String> = layers
            .iter()
            .map(|l| {
                format!(
                    r#"{{"mediaType":"{layer_type}","digest":"{}","size":{}}}"#,
                    Digest::of(l),
                    l.len()
                )
            })
            .collect();
        let artifact_type = artifact_type
            .map(|t| format!(r#""artifactType":"{t}","#))
            .unwrap_or_default();
        let manifest = format!(
            r#"{{"schemaVersion":2,"mediaType":"{manifest_type}",{artifact_type}"config":{{"mediaType":"{config_type}","digest":"{}","size":{}}},"layers":[{}]}}"#,
            Digest::of(&config),
            config.len(),
            descriptors.join(",")
        )
        .into_bytes();
        Self {
            digest: Digest::of(&manifest),
            manifest,
            config,
            layers,
        }
    }
}

/// How the registry behaves; a test changes it while the registry runs.
#[derive(Default)]
pub struct Behaviour {
    /// Answer 401 with a challenge until a request carries this token.
    pub token: Option<String>,
    /// The token endpoint's answer body.
    pub token_answer: Option<String>,
    /// The token endpoint's status.
    pub token_status: Option<u16>,
    /// The realm the challenge names; the registry's own `/token` if `None`.
    pub realm: Option<String>,
    /// A challenge with this scheme instead of Bearer.
    pub challenge_scheme: Option<String>,
    /// Always 401.
    pub refuse_token: bool,
    /// 401 with no challenge header.
    pub no_challenge: bool,
    /// The challenge names no scope.
    pub challenge_no_scope: bool,
    /// The challenge names no service.
    pub challenge_no_service: bool,
    /// The default realm has a query already.
    pub realm_query: bool,
    /// Answer blob requests with a redirect to this (`{digest}` is the blob's).
    pub redirect_blobs: Option<String>,
    /// Redirect every path to itself.
    pub redirect_loop: bool,
    /// A redirect with no Location.
    pub redirect_without_location: bool,
    /// Send the headers of a blob and then nothing.
    pub stall_body: bool,
    /// Read the request and never answer.
    pub stall_head: bool,
    /// Status for every request.
    pub status: Option<u16>,
    /// Read the request and close the connection without answering.
    pub hang_up: bool,
    /// Promise a blob's bytes and a hundred more, send the blob, and close.
    pub short_body: bool,
    /// Send manifests and blobs chunked, with a trailer after the last chunk.
    pub trailers: bool,
    /// Read a request for a redirect target (`/cdn/...`) and close without answering.
    pub hang_up_cdn: bool,
}

#[derive(Default)]
struct State {
    blobs: HashMap<String, Vec<u8>>,
    manifests: HashMap<String, (String, Vec<u8>)>,
    log: Vec<String>,
    behaviour: Behaviour,
}

pub struct Fixture {
    pub port: u16,
    pub cert: CertificateDer<'static>,
    state: Arc<Mutex<State>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Fixture {
    pub async fn start() -> Self {
        let names = [REGISTRY, OTHER, "127.0.0.1"].map(str::to_owned).to_vec();
        Self::start_named(names).await
    }

    /// A fixture whose certificate is valid for exactly `names`.
    pub async fn start_named(names: Vec<String>) -> Self {
        let made = rcgen::generate_simple_self_signed(names).expect("certificate");
        let cert = made.cert.der().clone();
        let key = PrivateKeyDer::from(PrivatePkcs8KeyDer::from(made.signing_key.serialize_der()));
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("versions")
        .with_no_client_auth()
        .with_single_cert(vec![cert.clone()], key)
        .expect("server config");
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let state = Arc::new(Mutex::new(State::default()));
        let shared = Arc::clone(&state);
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let state = Arc::clone(&shared);
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    if let Ok(stream) = acceptor.accept(stream).await {
                        serve(stream, state).await;
                    }
                });
            }
        });
        Self {
            port,
            cert,
            state,
            task,
        }
    }

    /// A socket that accepts connections and says nothing: a handshake that never ends.
    pub async fn silent() -> (u16, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let task = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                held.push(stream);
            }
        });
        (port, task)
    }

    /// The `host:port` the fixture is reached at under `name`.
    pub fn at(&self, name: &str) -> String {
        format!("{name}:{}", self.port)
    }

    /// The location of `repository` on the fixture as `registry.test`.
    pub fn location(&self, repository: &str) -> Location {
        Location::parse(&format!("{}/{repository}", self.at(REGISTRY))).expect("location")
    }

    /// A client that trusts the fixture, resolves its test names to loopback, and lists
    /// `registry.test` under `team/` as a private registry.
    pub fn client(&self) -> Client {
        self.client_for(&[&format!("{}/team", self.at(REGISTRY))], true)
    }

    /// A client with these allow-list entries, all private or all public.
    pub fn client_for(&self, entries: &[&str], private: bool) -> Client {
        let allow = entries
            .iter()
            .map(|e| {
                let entry = Allow::parse(e).expect("entry");
                if private {
                    entry.private().loopback()
                } else {
                    entry
                }
            })
            .collect();
        self.client_with(allow)
    }

    /// A client with exactly this allow-list.
    pub fn client_with(&self, allow: Vec<Allow>) -> Client {
        Client::with_roots(Policy::new(allow), vec![self.cert.clone()])
            .with_resolved(REGISTRY, LOOPBACK)
            .with_resolved(OTHER, LOOPBACK)
            .with_resolved("rebind.test", LOOPBACK)
    }

    /// Publishes `artifact`: its manifest under its digest, and every blob.
    pub fn publish(&self, artifact: &Artifact) {
        let mut state = self.state.lock().expect("lock");
        state.manifests.insert(
            artifact.digest.to_string(),
            (
                "application/vnd.oci.image.manifest.v1+json".to_owned(),
                artifact.manifest.clone(),
            ),
        );
        state.blobs.insert(
            Digest::of(&artifact.config).to_string(),
            artifact.config.clone(),
        );
        for layer in &artifact.layers {
            state
                .blobs
                .insert(Digest::of(layer).to_string(), layer.clone());
        }
    }

    /// Serves `bytes` as the manifest of `digest`, whatever they hash to.
    pub fn publish_manifest_as(&self, digest: &Digest, bytes: &[u8]) {
        let mut state = self.state.lock().expect("lock");
        state.manifests.insert(
            digest.to_string(),
            (
                "application/vnd.oci.image.manifest.v1+json".to_owned(),
                bytes.to_vec(),
            ),
        );
    }

    /// Serves `bytes` as the blob `digest`, whatever they hash to.
    pub fn publish_blob_as(&self, digest: &Digest, bytes: &[u8]) {
        let mut state = self.state.lock().expect("lock");
        state.blobs.insert(digest.to_string(), bytes.to_vec());
    }

    pub fn behave(&self, change: impl FnOnce(&mut Behaviour)) {
        change(&mut self.state.lock().expect("lock").behaviour);
    }

    /// The requests so far, as `<host> GET <path> [<authorization>]`.
    pub fn requests(&self) -> Vec<String> {
        self.state.lock().expect("lock").log.clone()
    }
}

struct Request {
    path: String,
    authorization: Option<String>,
    host: String,
}

async fn read_request<S: tokio::io::AsyncRead + Unpin>(stream: &mut S) -> Option<Request> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let text = String::from_utf8_lossy(&buf).into_owned();
    let mut lines = text.lines();
    let path = lines.next()?.split(' ').nth(1)?.to_owned();
    let headers: Vec<(&str, &str)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k, v.trim()))
        .collect();
    let header = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| (*v).to_owned())
    };
    Some(Request {
        path,
        authorization: header("authorization"),
        host: header("host").unwrap_or_default(),
    })
}

fn response(status: u16, headers: &[(&str, String)], body: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (k, v) in headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str("\r\n");
    let mut out = out.into_bytes();
    out.extend_from_slice(body);
    out
}

async fn serve<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
    mut stream: S,
    state: Arc<Mutex<State>>,
) {
    let Some(request) = read_request(&mut stream).await else {
        return;
    };
    let answer = {
        let mut s = state.lock().expect("lock");
        let auth = request.authorization.clone().unwrap_or_default();
        s.log
            .push(format!("{} GET {} [{auth}]", request.host, request.path));
        answer(&s, &request)
    };
    match answer {
        Answer::Bytes(bytes) => {
            let _ = stream.write_all(&bytes).await;
            let _ = stream.shutdown().await;
        }
        Answer::StallBody(head) => {
            let _ = stream.write_all(&head).await;
            let _ = stream.flush().await;
            std::future::pending::<()>().await;
        }
        Answer::StallHead => std::future::pending::<()>().await,
        Answer::HangUp => {}
    }
}

/// `bytes` as a chunked answer with a trailer after the last chunk.
fn chunked(bytes: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 200 X\r\nTransfer-Encoding: chunked\r\nTrailer: X-Sum\r\nConnection: close\r\n\r\n{:x}\r\n",
        bytes.len()
    )
    .into_bytes();
    out.extend_from_slice(bytes);
    out.extend_from_slice(b"\r\n0\r\nX-Sum: 1\r\n\r\n");
    out
}

enum Answer {
    Bytes(Vec<u8>),
    HangUp,
    StallBody(Vec<u8>),
    StallHead,
}

fn answer(s: &State, request: &Request) -> Answer {
    let b = &s.behaviour;
    let path = request.path.as_str();
    if b.stall_head {
        return Answer::StallHead;
    }
    if b.hang_up || (b.hang_up_cdn && path.starts_with("/cdn/")) {
        return Answer::HangUp;
    }
    if let Some(status) = b.status {
        return Answer::Bytes(response(status, &[], b"nope"));
    }
    if b.redirect_loop {
        return Answer::Bytes(response(307, &[("Location", path.to_owned())], b""));
    }
    if b.redirect_without_location {
        return Answer::Bytes(response(307, &[], b""));
    }
    if path.starts_with("/token") {
        if let Some(status) = b.token_status {
            return Answer::Bytes(response(status, &[], b"no token"));
        }
        let body = b
            .token_answer
            .clone()
            .unwrap_or_else(|| format!(r#"{{"token":"{}"}}"#, b.token.clone().unwrap_or_default()));
        return Answer::Bytes(response(200, &[], body.as_bytes()));
    }
    if let Some(target) = path.strip_prefix("/cdn/") {
        let digest = target.split('?').next().unwrap_or_default();
        return Answer::Bytes(match s.blobs.get(digest) {
            Some(bytes) => response(200, &[], bytes),
            None => response(404, &[], b""),
        });
    }
    if b.token.is_some() || b.refuse_token {
        let wanted = b.token.as_ref().map(|t| format!("Bearer {t}"));
        if b.refuse_token || request.authorization != wanted {
            return Answer::Bytes(challenge(b, request));
        }
    }
    if let Some((_, rest)) = path.split_once("/manifests/") {
        return match s.manifests.get(rest) {
            Some((_, bytes)) if b.trailers => Answer::Bytes(chunked(bytes)),
            Some((media, bytes)) => {
                Answer::Bytes(response(200, &[("Content-Type", media.clone())], bytes))
            }
            None => Answer::Bytes(response(404, &[], b"")),
        };
    }
    if let Some((_, digest)) = path.split_once("/blobs/") {
        if let Some(to) = &b.redirect_blobs {
            let to = to.replace("{digest}", digest);
            return Answer::Bytes(response(307, &[("Location", to)], b""));
        }
        return match s.blobs.get(digest) {
            Some(bytes) if b.trailers => Answer::Bytes(chunked(bytes)),
            Some(bytes) if b.short_body || b.stall_body => {
                let head = format!(
                    "HTTP/1.1 200 X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    bytes.len() + 100
                );
                if b.stall_body {
                    Answer::StallBody(head.into_bytes())
                } else {
                    Answer::Bytes([head.as_bytes(), bytes].concat())
                }
            }
            Some(bytes) => Answer::Bytes(response(200, &[], bytes)),
            None => Answer::Bytes(response(404, &[], b"")),
        };
    }
    Answer::Bytes(response(404, &[], b""))
}

/// The 401 that asks for a token.
fn challenge(b: &Behaviour, request: &Request) -> Vec<u8> {
    let scheme = b.challenge_scheme.as_deref().unwrap_or("Bearer");
    let service = if b.challenge_no_service {
        ""
    } else {
        r#",service="fake-registry""#
    };
    let scope = if b.challenge_no_scope {
        ""
    } else {
        r#",scope="repository:team/bundle:pull""#
    };
    let query = if b.realm_query { "?client=js" } else { "" };
    let realm = b
        .realm
        .clone()
        .unwrap_or_else(|| format!("https://{}/token{query}", request.host));
    let mut headers = Vec::new();
    if !b.no_challenge {
        headers.push((
            "WWW-Authenticate",
            format!(r#"{scheme} realm="{realm}"{service}{scope}"#),
        ));
    }
    response(401, &headers, b"")
}

/// Where the layout directory `root` keeps the blob `digest`.
pub fn blob_path(root: &Path, digest: &Digest) -> PathBuf {
    root.join("blobs").join("sha256").join(digest.hex())
}

/// Writes `artifact` as an OCI image-layout directory at `root` (which exists): the
/// `oci-layout` marker, an `index.json` listing its manifest, and every blob under its
/// digest.
pub fn write_layout(root: &Path, artifact: &Artifact) {
    std::fs::create_dir_all(root.join("blobs").join("sha256")).expect("blobs");
    std::fs::write(root.join("oci-layout"), r#"{"imageLayoutVersion":"1.0.0"}"#)
        .expect("oci-layout");
    write_index(root, &[&artifact.digest]);
    let blobs = [&artifact.manifest, &artifact.config]
        .into_iter()
        .chain(&artifact.layers);
    for blob in blobs {
        std::fs::write(blob_path(root, &Digest::of(blob)), blob).expect("blob");
    }
}

/// Writes an `index.json` that lists `manifests`.
pub fn write_index(root: &Path, manifests: &[&Digest]) {
    let entries: Vec<String> = manifests
        .iter()
        .map(|d| {
            format!(
                r#"{{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"{d}","size":1}}"#
            )
        })
        .collect();
    std::fs::write(
        root.join("index.json"),
        format!(
            r#"{{"schemaVersion":2,"manifests":[{}]}}"#,
            entries.join(",")
        ),
    )
    .expect("index.json");
}
