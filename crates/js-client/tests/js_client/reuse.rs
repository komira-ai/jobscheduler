//! The connection kept between requests: a client sends its next request on the connection
//! of the last one while the server keeps it open, and opens a new one when the server
//! closed it, said it would, or the last reply was not read to its end.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use js_client::{Client, ClientError};
use rustls::{ServerConfig, ServerConnection, StreamOwned};
use serde_json::json;

use crate::support::{Cert, Scratch};

const LIMIT: Duration = Duration::from_secs(20);

/// What the server does once it has answered a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Then {
    /// Keeps the connection open for the next request.
    Keep,
    /// Closes the connection, as a server does when its idle limit passes or it drains.
    Hangup,
    /// Says `connection: close` in the reply but keeps the socket open, reading on.
    SayClose,
    /// Answers with a body one byte over the client's limit, and keeps the connection.
    Oversized,
}

/// What the server saw of one connection.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Seen {
    /// The DER of each certificate the client presented in the connection's handshake.
    pub presented: Vec<Vec<u8>>,
    /// The request line of each request sent on the connection, in order.
    pub requests: Vec<String>,
}

/// What the server has seen so far: the connections it accepted, in order, and how many
/// requests it answered over all of them.
#[derive(Default)]
struct Log {
    connections: Vec<Seen>,
    answered: usize,
}

/// A server on 127.0.0.1 that keeps each connection open across requests (HTTP/1.1
/// keep-alive) and answers every request with an empty JSON object; the `n`th request it
/// answers, over all connections, is followed by `plan[n]` (and by [`Then::Keep`] past the
/// end of `plan`).
pub struct KeepAlive {
    addr: SocketAddr,
    log: Arc<Mutex<Log>>,
    hung_up: Receiver<()>,
}

impl KeepAlive {
    /// The server over plain http.
    pub fn plain(plan: Vec<Then>) -> Self {
        Self::start(None, plan)
    }

    /// The server inside TLS with `config`.
    pub fn tls(config: Arc<ServerConfig>, plan: Vec<Then>) -> Self {
        Self::start(Some(config), plan)
    }

    fn start(tls: Option<Arc<ServerConfig>>, plan: Vec<Then>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let log = Arc::new(Mutex::new(Log::default()));
        let (told, hung_up) = channel();
        let plan = Arc::new(plan);
        let shared = Arc::clone(&log);
        // The listener's thread outlives the test's use of it, as `hangup_url`'s does.
        thread::spawn(move || {
            for tcp in listener.incoming() {
                let Ok(tcp) = tcp else { continue };
                tcp.set_read_timeout(Some(LIMIT)).unwrap();
                let index = {
                    let mut log = shared.lock().unwrap_or_else(PoisonError::into_inner);
                    log.connections.push(Seen::default());
                    log.connections.len() - 1
                };
                let stream = match &tls {
                    None => Stream::Plain(tcp),
                    Some(config) => {
                        let conn = ServerConnection::new(Arc::clone(config)).unwrap();
                        Stream::Tls(Box::new(StreamOwned::new(conn, tcp)))
                    }
                };
                let (log, plan, told) = (Arc::clone(&shared), Arc::clone(&plan), told.clone());
                thread::spawn(move || serve(stream, index, &log, &plan, &told));
            }
        });
        Self { addr, log, hung_up }
    }

    /// The server's address with `scheme`.
    pub fn url(&self, scheme: &str) -> String {
        format!("{scheme}://{}", self.addr)
    }

    /// The connections accepted so far. Every request the client got an answer to is in it:
    /// the server writes it down before it answers.
    pub fn seen(&self) -> Vec<Seen> {
        let log = self.log.lock().unwrap_or_else(PoisonError::into_inner);
        log.connections.clone()
    }

    /// Waits until the server has closed the connection after a [`Then::Hangup`].
    pub fn wait_hung_up(&self) {
        self.hung_up
            .recv_timeout(LIMIT)
            .expect("the server closed the connection");
    }
}

/// A connection the server speaks over.
enum Stream {
    Plain(TcpStream),
    Tls(Box<StreamOwned<ServerConnection, TcpStream>>),
}

impl Stream {
    /// The certificates the client presented: none over plain http.
    fn presented(&self) -> Vec<Vec<u8>> {
        match self {
            Self::Plain(_) => Vec::new(),
            Self::Tls(tls) => tls
                .conn
                .peer_certificates()
                .map(|certs| certs.iter().map(|der| der.as_ref().to_vec()).collect())
                .unwrap_or_default(),
        }
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Plain(tcp) => tcp.read(buf),
            Self::Tls(tls) => tls.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Plain(tcp) => tcp.write(buf),
            Self::Tls(tls) => tls.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Plain(tcp) => tcp.flush(),
            Self::Tls(tls) => tls.flush(),
        }
    }
}

/// Answers the requests of one connection until the client closes it or the plan says to
/// end it.
fn serve(mut stream: Stream, index: usize, log: &Mutex<Log>, plan: &[Then], told: &Sender<()>) {
    while let Some(line) = read_request(&mut stream) {
        let then = {
            let mut log = log.lock().unwrap_or_else(PoisonError::into_inner);
            let n = log.answered;
            log.answered += 1;
            let presented = stream.presented();
            let seen = &mut log.connections[index];
            seen.presented = presented;
            seen.requests.push(line);
            plan.get(n).copied().unwrap_or(Then::Keep)
        };
        let reply = match then {
            Then::Keep | Then::Hangup => reply("{}", false),
            Then::SayClose => reply("{}", true),
            // The wire's bound on a reply body, plus one byte.
            Then::Oversized => reply(&" ".repeat((1 << 20) + 1), false),
        };
        // The client stops reading a reply over its limit and closes the connection.
        let _ = stream.write_all(&reply).and_then(|()| stream.flush());
        if then == Then::Hangup {
            drop(stream);
            let _ = told.send(());
            return;
        }
    }
}

/// A 200 reply with `body`, saying `connection: close` when `close`.
fn reply(body: &str, close: bool) -> Vec<u8> {
    let connection = if close { "connection: close\r\n" } else { "" };
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n{connection}\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// Reads one request, head and body, and returns its request line; `None` when the client
/// closed the connection (or it failed) before a whole request came.
fn read_request(stream: &mut Stream) -> Option<String> {
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
        .find_map(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = vec![0_u8; length];
    stream.read_exact(&mut body).ok()?;
    head.lines().next().map(str::to_owned)
}

fn token_file(dir: &Scratch) -> PathBuf {
    dir.write("token", b"tok-123\n", 0o600)
}

/// The request lines of each connection the server accepted.
fn requests(server: &KeepAlive) -> Vec<Vec<String>> {
    server
        .seen()
        .into_iter()
        .map(|seen| seen.requests)
        .collect()
}

/// Asserts that a client's requests in a row (every call, on the client and on a clone of
/// it, the way the agent's heartbeats go out one after another) ride one TCP connection
/// and so one TLS handshake, in the order they were made. It catches a client that opens
/// a connection for every request (a TCP and a TLS handshake each beat) and one whose
/// clones do not share the connection.
#[test]
fn requests_in_a_row_ride_one_connection_and_one_tls_handshake() {
    let dir = Scratch::new("reuse_tls");
    let cert = Cert::new();
    let ca = dir.write("ca.pem", cert.pem.as_bytes(), 0o644);
    let server = KeepAlive::tls(cert.server_config(), Vec::new());
    let client =
        Client::connect(&server.url("https"), &token_file(&dir), Some(&ca), LIMIT).unwrap();

    assert_eq!(client.get("/a").unwrap().status, 200);
    assert_eq!(client.post("/b", &json!({ "seq": 1 })).unwrap().status, 200);
    let within = client.post_within("/c", &json!({ "seq": 2 }), Duration::from_secs(5));
    assert_eq!(within.unwrap().status, 200);
    assert_eq!(client.clone().delete("/d").unwrap().status, 200);
    let with_body = client.delete_with("/e", &json!({ "id": 1 }));
    assert_eq!(with_body.unwrap().status, 200);

    assert_eq!(
        requests(&server),
        vec![vec![
            "GET /a HTTP/1.1".to_owned(),
            "POST /b HTTP/1.1".to_owned(),
            "POST /c HTTP/1.1".to_owned(),
            "DELETE /d HTTP/1.1".to_owned(),
            "DELETE /e HTTP/1.1".to_owned(),
        ]]
    );
}

/// Asserts that a connection the server closed while it sat idle (its idle limit passed,
/// it drained, it restarted) is not used again: the next request opens a new connection
/// and is answered, and the one after it rides that new connection. It catches a client
/// that sends a request on a connection the server has closed, which fails the request.
#[test]
fn a_connection_the_server_closed_is_not_used_again() {
    let dir = Scratch::new("reuse_hangup");
    let server = KeepAlive::plain(vec![Then::Hangup]);
    let client = Client::connect(&server.url("http"), &token_file(&dir), None, LIMIT).unwrap();

    assert_eq!(client.post("/a", &json!({})).unwrap().status, 200);
    server.wait_hung_up();
    assert_eq!(client.post("/b", &json!({})).unwrap().status, 200);
    assert_eq!(client.post("/c", &json!({})).unwrap().status, 200);

    assert_eq!(
        requests(&server),
        vec![
            vec!["POST /a HTTP/1.1".to_owned()],
            vec!["POST /b HTTP/1.1".to_owned(), "POST /c HTTP/1.1".to_owned()],
        ]
    );
}

/// Asserts that a reply saying `connection: close` (the lease server's answer to the last
/// request it takes on a connection) ends the connection on the client's side too, even
/// when the server leaves the socket open: the next request opens a new connection. It
/// catches a client that keeps a connection the server said it would close, whose next
/// request the server would never read.
#[test]
fn a_reply_that_says_close_ends_the_connection() {
    let dir = Scratch::new("reuse_say_close");
    let server = KeepAlive::plain(vec![Then::Keep, Then::SayClose]);
    let client = Client::connect(&server.url("http"), &token_file(&dir), None, LIMIT).unwrap();

    for path in ["/a", "/b", "/c"] {
        assert_eq!(client.get(path).unwrap().status, 200);
    }

    assert_eq!(
        requests(&server),
        vec![
            vec!["GET /a HTTP/1.1".to_owned(), "GET /b HTTP/1.1".to_owned()],
            vec!["GET /c HTTP/1.1".to_owned()],
        ]
    );
}

/// Asserts that a reply the client stopped reading (its body over the client's limit) does
/// not leave its connection to the next request: that request opens a new connection and
/// is answered. It catches a client that takes the unread rest of the last reply for the
/// head of the next one.
#[test]
fn a_reply_read_only_in_part_ends_the_connection() {
    let dir = Scratch::new("reuse_oversized");
    let server = KeepAlive::plain(vec![Then::Oversized]);
    let client = Client::connect(&server.url("http"), &token_file(&dir), None, LIMIT).unwrap();

    let refused = client.get("/a").expect_err("the reply is over the limit");
    assert!(matches!(refused, ClientError::Server(_)), "{refused}");
    let next = client.get("/b").unwrap();
    assert_eq!((next.status, next.body), (200, json!({})));

    assert_eq!(
        requests(&server),
        vec![
            vec!["GET /a HTTP/1.1".to_owned()],
            vec!["GET /b HTTP/1.1".to_owned()],
        ]
    );
}
