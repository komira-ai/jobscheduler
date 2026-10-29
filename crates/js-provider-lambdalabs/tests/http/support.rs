//! Scratch files, test certificates and a server on 127.0.0.1 that answers canned replies
//! by route and records what it was asked. Nothing here dials anything else.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rustls::crypto::ring::default_provider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ServerConfig, ServerConnection, StreamOwned};

/// A limit generous enough that a test never meets it by accident.
pub const PATIENT: Duration = Duration::from_secs(30);

/// The fixture `name`, as text.
pub fn fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    fs::read_to_string(path).unwrap()
}

/// A directory of files for one test, removed when it is dropped.
pub struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    /// An empty directory named `name`.
    pub fn new(name: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("js-provider-lambdalabs")
            .join(format!("{name}-{}", NEXT.fetch_add(1, Ordering::SeqCst)));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Self { dir }
    }

    /// The path of the entry `name`.
    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    /// Writes `bytes` to the file `name` with `mode` and returns its path.
    pub fn write(&self, name: &str, bytes: &[u8], mode: u32) -> PathBuf {
        let path = self.path(name);
        fs::write(&path, bytes).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    /// A key file holding `key` and a newline, mode 0600.
    pub fn key_file(&self, key: &str) -> PathBuf {
        self.write("key", format!("{key}\n").as_bytes(), 0o600)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// A self-signed certificate for 127.0.0.1 and its key.
pub struct Cert {
    /// The certificate in PEM, for a CA bundle file.
    pub pem: String,
    der: CertificateDer<'static>,
    key: Vec<u8>,
}

impl Cert {
    /// A new certificate.
    pub fn new() -> Self {
        let made = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).unwrap();
        Self {
            pem: made.cert.pem(),
            der: made.cert.der().clone(),
            key: made.signing_key.serialize_der(),
        }
    }

    fn server_config(&self) -> Arc<ServerConfig> {
        let key = PrivateKeyDer::from(PrivatePkcs8KeyDer::from(self.key.clone()));
        let config = ServerConfig::builder_with_provider(Arc::new(default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![self.der.clone()], key)
            .unwrap();
        Arc::new(config)
    }
}

/// What the server does with a request.
#[derive(Clone)]
pub enum Reply {
    /// Answers `status` with a JSON body and these extra headers.
    Json(u16, String, Vec<(String, String)>),
    /// Sends these bytes as they are.
    Raw(Vec<u8>),
    /// Holds the connection open and says nothing.
    Hang,
}

impl Reply {
    /// `status` with a JSON `body`.
    pub fn json(status: u16, body: &str) -> Self {
        Self::Json(status, body.to_owned(), Vec::new())
    }

    /// The same with one more header.
    pub fn with_header(self, name: &str, value: &str) -> Self {
        match self {
            Self::Json(status, body, mut headers) => {
                headers.push((name.to_owned(), value.to_owned()));
                Self::Json(status, body, headers)
            }
            other => other,
        }
    }

    fn bytes(&self) -> Vec<u8> {
        match self {
            Self::Json(status, body, headers) => {
                let extra: String = headers
                    .iter()
                    .map(|(name, value)| format!("{name}: {value}\r\n"))
                    .collect();
                format!(
                    "HTTP/1.1 {status} Reply\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n{extra}\r\n{body}",
                    body.len()
                )
                .into_bytes()
            }
            Self::Raw(bytes) => bytes.clone(),
            Self::Hang => Vec::new(),
        }
    }
}

/// One request the server saw.
#[derive(Debug, Clone)]
pub struct Seen {
    /// The method.
    pub method: String,
    /// The path and query, as sent.
    pub target: String,
    /// The headers, names in lower case.
    pub headers: Vec<(String, String)>,
    /// The body, as many bytes as `content-length` said.
    pub body: Vec<u8>,
}

impl Seen {
    /// The value of the header `name`.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

#[derive(Default)]
struct Shared {
    routes: Mutex<HashMap<String, VecDeque<Reply>>>,
    seen: Mutex<Vec<Seen>>,
    held: Mutex<Vec<TcpStream>>,
    stop: AtomicBool,
}

/// A server on 127.0.0.1 answering canned replies by `METHOD target`. A route's replies
/// are used in order, the last one for every request after; a request with no route gets
/// a 500 that says so.
pub struct Server {
    addr: SocketAddr,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Server {
    /// A plain http server.
    pub fn plain() -> Self {
        Self::start(None)
    }

    /// A server inside TLS with `cert`.
    pub fn tls(cert: &Cert) -> Self {
        Self::start(Some(cert.server_config()))
    }

    fn start(tls: Option<Arc<ServerConfig>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let shared = Arc::new(Shared::default());
        let thread = {
            let shared = Arc::clone(&shared);
            thread::spawn(move || {
                for stream in listener.incoming() {
                    if shared.stop.load(Ordering::SeqCst) {
                        return;
                    }
                    if let Ok(tcp) = stream {
                        serve(tcp, tls.as_ref(), &shared);
                    }
                }
            })
        };
        Self {
            addr,
            shared,
            thread: Some(thread),
        }
    }

    /// The server's base URL with `scheme`.
    pub fn url(&self, scheme: &str) -> String {
        format!("{scheme}://{}", self.addr)
    }

    /// Adds `reply` to the route `route` (such as `GET /api/v1/instances`).
    pub fn on(&self, route: &str, reply: Reply) -> &Self {
        self.shared
            .routes
            .lock()
            .unwrap()
            .entry(route.to_owned())
            .or_default()
            .push_back(reply);
        self
    }

    /// What the server has been asked so far.
    pub fn seen(&self) -> Vec<Seen> {
        self.shared.seen.lock().unwrap().clone()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        // Wake the accept loop so it sees the flag.
        let _ = TcpStream::connect(self.addr);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A stream the server speaks over.
trait Duplex: Read + Write {}

impl Duplex for TcpStream {}

impl Duplex for StreamOwned<ServerConnection, TcpStream> {}

fn serve(tcp: TcpStream, tls: Option<&Arc<ServerConfig>>, shared: &Shared) {
    tcp.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let held = tcp.try_clone().unwrap();
    let hang = match tls {
        None => exchange(&mut { tcp }, shared),
        Some(config) => {
            let conn = ServerConnection::new(Arc::clone(config)).unwrap();
            exchange(&mut StreamOwned::new(conn, tcp), shared)
        }
    };
    if hang {
        shared.held.lock().unwrap().push(held);
    }
}

/// Reads one request, answers it, and says whether the connection is to be held open.
fn exchange(stream: &mut dyn Duplex, shared: &Shared) -> bool {
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        match stream.read(&mut byte) {
            Ok(1) => head.push(byte[0]),
            _ => return false,
        }
    }
    let head = String::from_utf8_lossy(&head).into_owned();
    let mut lines = head.lines();
    let mut first = lines.next().unwrap_or_default().split(' ');
    let method = first.next().unwrap_or_default().to_owned();
    let target = first.next().unwrap_or_default().to_owned();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    let length = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = vec![0_u8; length];
    if stream.read_exact(&mut body).is_err() {
        return false;
    }
    let route = format!("{method} {target}");
    shared.seen.lock().unwrap().push(Seen {
        method,
        target,
        headers,
        body,
    });
    let reply = {
        let mut routes = shared.routes.lock().unwrap();
        match routes.get_mut(&route) {
            Some(queue) if queue.len() > 1 => queue.pop_front(),
            Some(queue) => queue.front().cloned(),
            None => None,
        }
    }
    .unwrap_or_else(|| Reply::json(500, &format!("no route: {route}")));
    let _ = stream.write_all(&reply.bytes());
    let _ = stream.flush();
    matches!(reply, Reply::Hang)
}

/// The URL of a server that closes every connection without a reply. The listener stays
/// bound for the life of the test process: a port that was bound and then freed can be
/// taken by another process before the client dials it.
pub fn hangup_url() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        for stream in listener.incoming() {
            drop(stream);
        }
    });
    format!("http://{addr}")
}
