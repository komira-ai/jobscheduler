//! Scratch files, test certificates and a server that answers canned replies.

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use js_testkit::dir::TestDir;
use rustls::crypto::ring::default_provider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::server::WebPkiClientVerifier;
use rustls::{RootCertStore, ServerConfig, ServerConnection, StreamOwned};

/// How long the fake server waits for a request before it gives up on a connection.
const SERVER_PATIENCE: Duration = Duration::from_secs(20);

/// A directory of files for one test, removed when it is dropped.
pub struct Scratch {
    dir: PathBuf,
    _owner: TestDir,
}

impl Scratch {
    /// An empty directory named `name`.
    pub fn new(name: &str) -> Self {
        let owner = TestDir::under(
            Path::new(env!("CARGO_TARGET_TMPDIR")).join("js-client"),
            name,
        );
        Self {
            dir: owner.path().to_path_buf(),
            _owner: owner,
        }
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
}

/// Asserts that two scratch directories made under one name are two directories: the
/// second one's set-up does not remove the first one's files and dropping the second does
/// not remove the first. It catches a path named by the test alone, which two tests of
/// one name, or two runs of one test binary at once, would share and delete under each
/// other.
#[test]
fn scratches_of_one_name_are_separate_and_each_removes_only_itself() {
    let first = Scratch::new("same-name");
    first.write("f", b"one", 0o600);
    let second = Scratch::new("same-name");
    assert_ne!(first.path("f"), second.path("f"));
    assert!(
        first.path("f").exists(),
        "a second scratch removed the first"
    );
    second.write("f", b"two", 0o600);
    let kept = first.path("f");
    drop(second);
    assert_eq!(fs::read(kept).unwrap(), b"one");
}

/// A self-signed certificate for 127.0.0.1 and its key.
pub struct Cert {
    /// The certificate in PEM, for `--ca-cert`.
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

impl Cert {
    /// A server configuration with this certificate that refuses a client unless it
    /// presents a certificate issued by `client_ca`.
    pub fn requiring_clients_of(&self, client_ca: CertificateDer<'static>) -> Arc<ServerConfig> {
        let mut roots = RootCertStore::empty();
        roots.add(client_ca).unwrap();
        let provider = Arc::new(default_provider());
        let verifier =
            WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider.clone())
                .build()
                .unwrap();
        let key = PrivateKeyDer::from(PrivatePkcs8KeyDer::from(self.key.clone()));
        let config = ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_client_cert_verifier(verifier)
            .with_single_cert(vec![self.der.clone()], key)
            .unwrap();
        Arc::new(config)
    }
}

/// A reply with `status` and a JSON `body`.
pub fn json_reply(status: u16, body: &str) -> Vec<u8> {
    text_reply(status, "application/json", body)
}

/// A reply with `status` and a `body` of the given content type.
pub fn text_reply(status: u16, content_type: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status} Reply\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// A server on 127.0.0.1 that answers its connections, one canned reply each, in order.
pub struct Fake {
    addr: SocketAddr,
    served: JoinHandle<Vec<String>>,
}

impl Fake {
    /// A plain http server answering `replies`.
    pub fn plain(replies: Vec<Vec<u8>>) -> Self {
        Self::start(replies, None)
    }

    /// A server answering `replies` inside TLS with `cert`.
    pub fn tls(replies: Vec<Vec<u8>>, cert: &Cert) -> Self {
        Self::start(replies, Some(cert.server_config()))
    }

    fn start(replies: Vec<Vec<u8>>, tls: Option<Arc<ServerConfig>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let served = thread::spawn(move || {
            let mut seen = Vec::new();
            for reply in &replies {
                let (tcp, _) = listener.accept().unwrap();
                tcp.set_read_timeout(Some(SERVER_PATIENCE)).unwrap();
                seen.push(serve_one(tcp, tls.as_ref(), reply));
            }
            seen
        });
        Self { addr, served }
    }

    /// The server's address with `scheme`.
    pub fn url(&self, scheme: &str) -> String {
        format!("{scheme}://{}", self.addr)
    }

    /// Waits for every reply to be sent and returns the requests received (head and
    /// body), one per connection; a connection that sent no request reads as "".
    pub fn requests(self) -> Vec<String> {
        self.served.join().unwrap()
    }
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

/// A stream the fake server speaks over.
trait Duplex: Read + Write {}

impl Duplex for TcpStream {}

impl Duplex for StreamOwned<ServerConnection, TcpStream> {}

fn serve_one(tcp: TcpStream, tls: Option<&Arc<ServerConfig>>, reply: &[u8]) -> String {
    match tls {
        None => exchange(&mut { tcp }, reply),
        Some(config) => {
            let conn = ServerConnection::new(Arc::clone(config)).unwrap();
            exchange(&mut StreamOwned::new(conn, tcp), reply)
        }
    }
}

/// Reads one request, sends `reply`, and returns the request as text.
fn exchange(stream: &mut dyn Duplex, reply: &[u8]) -> String {
    let Some(request) = read_request(stream) else {
        return String::new();
    };
    // The client may stop reading a reply it refuses (one over its limit).
    let _ = stream.write_all(reply);
    let _ = stream.flush();
    request.head + &String::from_utf8_lossy(&request.body)
}

/// One request as a fake server read it.
pub struct Request {
    /// The request line and headers, up to and including the blank line that ends them.
    pub head: String,
    /// The bytes that followed the head, as many as its `content-length` names (none when
    /// it names none), or fewer when the stream ended or failed first.
    pub body: Vec<u8>,
}

/// Reads one request from `stream`: its head, then the body its `content-length` names,
/// and nothing after it. `None` when the stream ends or fails before the head does.
///
/// A fake reads the whole body before it answers and closes: a socket closed with bytes
/// still unread sends the client a reset instead of a close, and the client can see the
/// reset before it reads the answer, or while it is still sending the body.
pub fn read_request<R: Read + ?Sized>(stream: &mut R) -> Option<Request> {
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        match stream.read(&mut byte) {
            Ok(1) => head.push(byte[0]),
            _ => return None,
        }
    }
    let head = String::from_utf8_lossy(&head).into_owned();
    let length = head
        .lines()
        .filter_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(str::to_owned)
        })
        .find_map(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(0);
    let mut body = Vec::new();
    let _ = Read::take(&mut *stream, length).read_to_end(&mut body);
    Some(Request { head, body })
}

/// Asserts that a request is read up to the end of the body its `content-length` names
/// (a header name in any case) and no further, so a byte after it stays unread.
#[test]
fn a_request_is_read_to_the_end_of_its_body_and_no_further() {
    let mut stream: &[u8] = b"POST /a HTTP/1.1\r\nContent-Length: 5\r\n\r\nhello!";
    let request = read_request(&mut stream).unwrap();
    assert_eq!(
        request.head,
        "POST /a HTTP/1.1\r\nContent-Length: 5\r\n\r\n"
    );
    assert_eq!(request.body, b"hello");
    assert_eq!(stream, b"!");
}

/// Asserts that a request whose head names no length is read as having no body.
#[test]
fn a_request_with_no_content_length_has_no_body() {
    let mut stream: &[u8] = b"GET /a HTTP/1.1\r\nhost: x\r\n\r\nnext";
    let request = read_request(&mut stream).unwrap();
    assert_eq!(request.head, "GET /a HTTP/1.1\r\nhost: x\r\n\r\n");
    assert_eq!(request.body, b"");
    assert_eq!(stream, b"next");
}

/// Asserts that a stream that ends inside the head is no request, and one that ends inside
/// the body gives the body bytes that arrived.
#[test]
fn a_stream_that_ends_early_gives_what_arrived() {
    let mut cut_in_head: &[u8] = b"POST /a HTTP/1.1\r\ncontent-length: 5\r\n";
    assert!(read_request(&mut cut_in_head).is_none());
    let mut cut_in_body: &[u8] = b"POST /a HTTP/1.1\r\ncontent-length: 5\r\n\r\nhel";
    assert_eq!(read_request(&mut cut_in_body).unwrap().body, b"hel");
}
