//! A server on 127.0.0.1 that answers canned replies by `METHOD path` and records what it
//! was asked, and the defaults that make it a Tailscale API. Nothing here dials anything
//! else.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// The access token the default token route hands out.
pub const TOKEN: &str = "tskey-api-TOKENMARK-41c7";

/// The auth key the default key route hands out.
pub const AUTH_KEY: &str = "tskey-auth-KEYMARK-9f3a";

/// The OAuth client secret the tests use.
pub const CLIENT_SECRET: &str = "tskey-client-SECRETMARK-77d2";

/// What the server does with a request.
#[derive(Clone)]
pub enum Reply {
    /// Answers `status` with a JSON body.
    Json(u16, String),
    /// Sends these bytes as they are.
    Raw(Vec<u8>),
    /// Holds the connection open and says nothing.
    Hang,
}

impl Reply {
    /// `status` with `body`.
    pub fn json(status: u16, body: &str) -> Self {
        Self::Json(status, body.to_owned())
    }

    fn bytes(&self) -> Vec<u8> {
        match self {
            Self::Json(status, body) => format!(
                "HTTP/1.1 {status} Reply\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )
            .into_bytes(),
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
    /// The path.
    pub target: String,
    /// The headers, names in lower case.
    pub headers: Vec<(String, String)>,
    /// The body.
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

    /// The body as text.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// The body as JSON.
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap()
    }
}

#[derive(Default)]
struct Shared {
    routes: Mutex<HashMap<String, VecDeque<Reply>>>,
    seen: Mutex<Vec<Seen>>,
    held: Mutex<Vec<TcpStream>>,
    stop: AtomicBool,
}

/// A server answering canned replies by `METHOD path`. A route's replies are used in
/// order, the last one for every request after; a request with no route gets a 404.
pub struct Fake {
    addr: SocketAddr,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Fake {
    /// A server with no routes.
    pub fn empty() -> Self {
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
                        serve(tcp, &shared);
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

    /// A Tailscale API for the tailnet `-` with one device named `host` and replies that
    /// work.
    pub fn tailscale(host: &str) -> Self {
        let fake = Self::empty();
        fake.on(
            "POST /api/v2/oauth/token",
            Reply::json(
                200,
                &format!(
                    r#"{{"access_token":"{TOKEN}","token_type":"Bearer","expires_in":3600,"scope":"auth_keys"}}"#
                ),
            ),
        );
        fake.on(
            "POST /api/v2/tailnet/-/keys",
            Reply::json(200, &format!(r#"{{"id":"k1","key":"{AUTH_KEY}"}}"#)),
        );
        fake.on(
            "GET /api/v2/tailnet/-/devices",
            Reply::json(
                200,
                &format!(
                    r#"{{"devices":[{{"id":"d1","hostname":"{host}","tags":["tag:js-agent"]}},{{"id":"d9","hostname":"other","tags":["tag:js-agent"]}}]}}"#
                ),
            ),
        );
        fake.on("DELETE /api/v2/device/d1", Reply::json(200, ""));
        fake
    }

    /// The server's base URL.
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Adds `reply` to the route `route` (such as `POST /api/v2/oauth/token`).
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

    /// Replaces the replies of `route` with `reply`.
    pub fn set(&self, route: &str, reply: Reply) -> &Self {
        self.shared.routes.lock().unwrap().remove(route);
        self.on(route, reply)
    }

    /// What the server has been asked so far.
    pub fn seen(&self) -> Vec<Seen> {
        self.shared.seen.lock().unwrap().clone()
    }

    /// How many requests the server has had for `route`.
    pub fn count(&self, route: &str) -> usize {
        self.seen()
            .iter()
            .filter(|s| format!("{} {}", s.method, s.target) == route)
            .count()
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        // Wake the accept loop so it sees the flag.
        let _ = TcpStream::connect(self.addr);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve(mut tcp: TcpStream, shared: &Shared) {
    tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let held = tcp.try_clone().unwrap();
    if exchange(&mut tcp, shared) {
        shared.held.lock().unwrap().push(held);
    }
}

/// Reads one request, answers it, and says whether the connection is to be held open.
fn exchange(stream: &mut TcpStream, shared: &Shared) -> bool {
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
    .unwrap_or_else(|| Reply::json(404, "{}"));
    let _ = stream.write_all(&reply.bytes());
    let _ = stream.flush();
    matches!(reply, Reply::Hang)
}

/// The base URL of a port nothing listens on.
pub fn closed_port_url() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{addr}")
}
