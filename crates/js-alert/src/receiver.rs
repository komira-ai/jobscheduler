//! A receiver on 127.0.0.1 for tests of code that POSTs: it records each request and
//! answers a fixed status. Nothing here dials anything.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;

/// One request a [`Receiver`] saw.
#[derive(Debug, Clone)]
pub struct Seen {
    /// The request line and the headers.
    pub head: String,
    /// The body.
    pub body: String,
}

/// A server that answers every request with one status and keeps the requests.
pub struct Receiver {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<Seen>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Receiver {
    /// Starts a receiver that answers `status` to everything, with no body.
    pub fn start(status: u16) -> Self {
        Self::start_with(status, "")
    }

    /// Starts a receiver that answers `status` and `body` to everything.
    pub fn start_with(status: u16, body: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (seen, stop) = (Arc::clone(&seen), Arc::clone(&stop));
            let reply = format!(
                "HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            std::thread::spawn(move || serve(&listener, &reply, &seen, &stop))
        };
        Self {
            addr,
            seen,
            stop,
            thread: Some(thread),
        }
    }

    /// The address it listens on.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The requests seen so far, in order.
    pub fn requests(&self) -> Vec<Seen> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wakes the accept that waits.
        let _ = TcpStream::connect(self.addr);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

fn serve(listener: &TcpListener, reply: &str, seen: &Mutex<Vec<Seen>>, stop: &AtomicBool) {
    for stream in listener.incoming() {
        let mut stream = stream.unwrap();
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let (head, body) = read_request(&mut stream);
        seen.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Seen { head, body });
        stream.write_all(reply.as_bytes()).unwrap();
    }
}

/// Reads one request: the head up to the blank line, and as many body bytes as its
/// `content-length` says (none without one).
fn read_request(stream: &mut TcpStream) -> (String, String) {
    let mut reader = BufReader::new(stream);
    let mut head = String::new();
    let mut length = 0;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" {
            break;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = value.trim().parse().unwrap();
        }
        head.push_str(&line);
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).unwrap();
    (head, String::from_utf8(body).unwrap())
}
