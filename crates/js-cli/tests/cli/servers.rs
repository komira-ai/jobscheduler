//! A server that stands between the CLI and the real lease server, to lose an answer after
//! the server has acted, or before it has.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;

/// What a proxy does with one request after it has passed it to the real server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fate {
    /// Passes the answer on.
    Deliver,
    /// Reads the answer and closes the connection without sending a byte of it.
    CloseAfter,
    /// Reads the answer and sends a 500 in its place.
    Answer500,
    /// Closes the connection without passing the request on: the server never sees it.
    CloseBefore,
}

/// A proxy to the real server that decides, by the request line (`POST /v1/leases`), what
/// becomes of each answer, and logs every request line.
pub struct Proxy {
    pub url: String,
    log: Arc<Mutex<Vec<String>>>,
}

impl Proxy {
    /// A proxy to `upstream` (an `http://` URL) that gives each request the fate `rule`
    /// names for its request line.
    pub fn to(upstream: &str, rule: impl Fn(&str) -> Fate + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let upstream = upstream.trim_start_matches("http://").to_owned();
        let (log, rule) = (Arc::new(Mutex::new(Vec::new())), Arc::new(rule));
        let seen = Arc::clone(&log);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let (upstream, seen, rule) = (upstream.clone(), Arc::clone(&seen), rule.clone());
                let stream = stream.unwrap();
                thread::spawn(move || pass(stream, &upstream, &seen, &*rule));
            }
        });
        Self { url, log }
    }

    /// The request lines the proxy has seen, in order.
    pub fn requests(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    /// How many starts (`POST /v1/leases`) it has seen.
    pub fn starts(&self) -> usize {
        let starts = self.requests();
        starts
            .iter()
            .filter(|line| *line == "POST /v1/leases")
            .count()
    }
}

/// Serves one connection of a proxy.
fn pass(
    mut client: TcpStream,
    upstream: &str,
    log: &Mutex<Vec<String>>,
    rule: &dyn Fn(&str) -> Fate,
) {
    let (head, body) = read_request(&mut client);
    let line = head.lines().next().unwrap().trim_end_matches(" HTTP/1.1");
    log.lock().unwrap().push(line.to_owned());
    let fate = rule(line);
    if fate == Fate::CloseBefore {
        return;
    }
    let mut server = TcpStream::connect(upstream).unwrap();
    let closing = head.replacen("\r\n", "\r\nConnection: close\r\n", 1);
    server.write_all(closing.as_bytes()).unwrap();
    server.write_all(&body).unwrap();
    let mut answer = Vec::new();
    server.read_to_end(&mut answer).unwrap();
    match fate {
        Fate::Deliver => {
            let _ = client.write_all(&answer);
        }
        Fate::Answer500 => {
            let _ = client
                .write_all(reply("500 Internal Server Error", r#"{"error":"boom"}"#).as_bytes());
        }
        Fate::CloseAfter | Fate::CloseBefore => {}
    }
}

/// Reads a request to the end of its body: the head as text, and the body.
fn read_request(stream: &mut impl Read) -> (String, Vec<u8>) {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap_or(0) == 1 {
        head.push(byte[0]);
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
    let mut body = vec![0u8; length];
    let _ = stream.read_exact(&mut body);
    (head, body)
}

/// A reply with this status line and JSON body, ended by closing the connection.
fn reply(status: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
}
