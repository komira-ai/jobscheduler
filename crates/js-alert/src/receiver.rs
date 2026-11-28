//! A receiver on 127.0.0.1 for tests of code that POSTs: it records each request and
//! answers a fixed status. Nothing here dials anything.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

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
        {
            let (seen, stop) = (Arc::clone(&seen), Arc::clone(&stop));
            let reply = format!(
                "HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            // Detached: the wake-up in `drop` ends it.
            std::thread::spawn(move || serve(&listener, &reply, &seen, &stop));
        }
        Self { addr, seen, stop }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts that `serve`, with `stop` set (as a receiver's drop sets it), returns at the
    /// first connection it accepts and records nothing of it. `serve` runs on the test's
    /// thread, so it has taken the stop when the assertion runs; a receiver's own thread takes
    /// it only after its drop, which nothing waits for. The connection sends a whole request,
    /// and the listener does not wait once that connection is taken: a `serve` that went on
    /// would record the request and then fail at its next accept. Catches a thread that keeps
    /// serving after its receiver was dropped.
    #[test]
    fn serve_returns_at_the_first_connection_once_stopped() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("binds loopback");
        let addr = listener.local_addr().expect("an address");
        let mut waiting = TcpStream::connect(addr).expect("connects");
        waiting
            .write_all(b"POST / HTTP/1.1\r\ncontent-length: 0\r\n\r\n")
            .expect("sends");
        listener.set_nonblocking(true).expect("nonblocking");
        let seen = Mutex::new(Vec::new());
        serve(&listener, "", &seen, &AtomicBool::new(true));
        assert_eq!(seen.lock().expect("lock").len(), 0);
    }
}
