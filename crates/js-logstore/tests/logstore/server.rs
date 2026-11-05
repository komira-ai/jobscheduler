//! A server on 127.0.0.1, port 0, that answers each connection with the next canned reply and
//! records what it was asked. Nothing here dials anything but it.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;

/// What the server does with the next connection.
pub enum Reply {
    /// Reads the request and writes these bytes.
    Raw(Vec<u8>),
    /// Reads the request and says nothing, holding the connection open.
    Hang,
}

impl Reply {
    /// An answer of `status` with `headers` and `body`, closing the connection after it.
    pub fn http(status: u16, headers: &[(&str, &str)], body: &[u8]) -> Self {
        let mut head = format!(
            "HTTP/1.1 {status} Reply\r\ncontent-length: {}\r\nconnection: close\r\n",
            body.len()
        );
        for (name, value) in headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("\r\n");
        let mut bytes = head.into_bytes();
        bytes.extend_from_slice(body);
        Self::Raw(bytes)
    }
}

/// One request the server read.
#[derive(Debug, Clone)]
pub struct Seen {
    /// The request line.
    pub line: String,
    /// The headers, names in lower case.
    pub headers: Vec<(String, String)>,
    /// The body, as long as `content-length` said.
    pub body: Vec<u8>,
}

impl Seen {
    /// The value of the header `name`.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(have, _)| have == name)
            .map(|(_, value)| value.as_str())
    }
}

/// The server. It serves one connection per reply, in order, then stops accepting.
pub struct Server {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<Seen>>>,
    held: Arc<Mutex<Vec<TcpStream>>>,
}

impl Server {
    /// A server answering `replies`.
    pub fn start(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let held = Arc::new(Mutex::new(Vec::new()));
        let (thread_seen, thread_held) = (Arc::clone(&seen), Arc::clone(&held));
        thread::spawn(move || {
            for reply in replies {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                if let Some(request) = read_request(&mut stream) {
                    thread_seen.lock().unwrap().push(request);
                }
                match reply {
                    Reply::Raw(bytes) => {
                        let _ = stream.write_all(&bytes);
                        let _ = stream.flush();
                    }
                    Reply::Hang => thread_held.lock().unwrap().push(stream),
                }
            }
        });
        Self { addr, seen, held }
    }

    /// The server's base URL.
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// The `host:port` it listens on.
    pub fn authority(&self) -> String {
        self.addr.to_string()
    }

    /// What the server has read so far.
    pub fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    /// How many connections it holds open.
    pub fn held(&self) -> usize {
        self.held.lock().unwrap().len()
    }
}

/// One request from `stream`: the head up to the blank line, then `content-length` bytes.
fn read_request(stream: &mut TcpStream) -> Option<Seen> {
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        match stream.read(&mut byte) {
            Ok(1) => head.push(byte[0]),
            _ => return None,
        }
    }
    let head = String::from_utf8_lossy(&head).into_owned();
    let mut lines = head.lines();
    let line = lines.next().unwrap_or_default().to_owned();
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
    stream.read_exact(&mut body).ok()?;
    Some(Seen {
        line,
        headers,
        body,
    })
}
