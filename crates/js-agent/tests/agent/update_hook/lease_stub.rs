//! A stand-in for the lease server in the lease port's transport tests. It answers each
//! connection as its script says, counts them, and shuts down on [`Stub::stop`] or on drop: it
//! wakes its accept loop with a connection from an address it noted before connecting, so every
//! connection queued before that one is counted, and joins its thread, which drops the
//! listener and every connection it held. A failed assertion therefore leaves nothing running.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream};
use std::os::fd::OwnedFd;
use std::sync::{Arc, Mutex, Weak};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rustix::net::{self, AddressFamily, SocketType};

/// How long the stub waits on a request that is being sent; the clients of these tests send
/// theirs at once. Not a pass/fail timeout: it keeps a broken client from holding the stub.
const READ_GUARD: Duration = Duration::from_secs(60);

/// What the stub does with one connection.
#[derive(Clone, Copy, Debug)]
pub(super) enum Reply {
    /// Takes the connection and never reads or answers it (a TLS client waits for the
    /// server's hello, a plain one for the head).
    Silent,
    /// Reads the request and sends a head that declares 100 bytes of body, then 10 of them,
    /// and nothing more.
    Partial,
    /// Reads the request and answers `status` with `body`.
    Answer(u16, &'static str),
}

/// A socket bound to a loopback port and never listening: a connection to it is refused, and
/// no other test can be handed its port while it lives.
pub(super) fn closed() -> (OwnedFd, String) {
    let socket = net::socket(AddressFamily::INET, SocketType::STREAM, None).unwrap();
    net::bind(&socket, &SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
    let addr = SocketAddrV4::try_from(net::getsockname(&socket).unwrap()).unwrap();
    (socket, format!("http://{addr}"))
}

/// The server: see the module header.
pub(super) struct Stub {
    /// `http://<addr>`.
    pub(super) url: String,
    addr: SocketAddrV4,
    wake: Arc<Mutex<Option<SocketAddr>>>,
    thread: Option<JoinHandle<usize>>,
    alive: Weak<()>,
}

impl Stub {
    /// Serves connection `n` as `script[n]`, and every one after the script as its last entry.
    pub(super) fn start(script: &[Reply]) -> Stub {
        assert!(!script.is_empty());
        let script = script.to_vec();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let SocketAddr::V4(addr) = listener.local_addr().unwrap() else {
            panic!("a loopback v4 listener has a v4 address");
        };
        let wake = Arc::new(Mutex::new(None));
        let seen = Arc::clone(&wake);
        let token = Arc::new(());
        let alive = Arc::downgrade(&token);
        let thread = thread::spawn(move || {
            let _alive = token;
            let mut held = Vec::new();
            let mut count = 0;
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                if Some(stream.peer_addr().unwrap()) == *seen.lock().unwrap() {
                    break;
                }
                let reply = script
                    .get(count)
                    .copied()
                    .unwrap_or(script[script.len() - 1]);
                count += 1;
                serve(&mut stream, reply);
                if !matches!(reply, Reply::Answer(..)) {
                    held.push(stream);
                }
            }
            count
        });
        Stub {
            url: format!("http://{addr}"),
            addr,
            wake,
            thread: Some(thread),
            alive,
        }
    }

    /// The address as `https://`, for a client that must first finish a TLS handshake.
    pub(super) fn https_url(&self) -> String {
        format!("https://{}", self.addr)
    }

    /// Alive while the accept thread runs, and with it the listener and the connections it
    /// holds.
    pub(super) fn alive(&self) -> Weak<()> {
        Weak::clone(&self.alive)
    }

    /// Stops the stub and says how many connections it served before the stop.
    pub(super) fn stop(mut self) -> usize {
        self.shutdown()
            .expect("the stub could not be woken")
            .join()
            .expect("the stub's thread panicked")
    }

    /// Notes the address of a new socket, then connects from it: the accept loop ends on that
    /// connection. The thread is handed back to join only when the wake-up was sent, so that a
    /// join never waits on a loop nothing will wake.
    fn shutdown(&mut self) -> Option<JoinHandle<usize>> {
        let thread = self.thread.take()?;
        let socket = net::socket(AddressFamily::INET, SocketType::STREAM, None).ok()?;
        net::bind(&socket, &SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).ok()?;
        let from = SocketAddrV4::try_from(net::getsockname(&socket).ok()?).ok()?;
        *self.wake.lock().ok()? = Some(SocketAddr::V4(from));
        net::connect(&socket, &self.addr).ok()?;
        Some(thread)
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        if let Some(thread) = self.shutdown() {
            // A panic of the thread has already failed the test that made it.
            let _ = thread.join();
        }
    }
}

/// Does what `reply` says with one connection.
fn serve(stream: &mut TcpStream, reply: Reply) {
    let answer = match reply {
        Reply::Silent => return,
        Reply::Partial => "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 100\r\n\r\n{\"id\":\"m\"".to_owned(),
        Reply::Answer(status, body) => format!(
            "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        ),
    };
    read_request(stream);
    // A client that has given up has closed its end; what it would have read does not matter.
    let _ = stream.write_all(answer.as_bytes());
}

/// Reads a request head and as many bytes of body as it declares, or until the client stops
/// sending.
fn read_request(stream: &mut TcpStream) {
    stream.set_read_timeout(Some(READ_GUARD)).unwrap();
    let mut seen = Vec::new();
    while !complete(&seen) {
        let mut chunk = [0_u8; 1024];
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => seen.extend_from_slice(&chunk[..n]),
        }
    }
}

/// Whether `seen` holds a request head and the body it declares.
fn complete(seen: &[u8]) -> bool {
    let text = String::from_utf8_lossy(seen);
    let Some((head, body)) = text.split_once("\r\n\r\n") else {
        return false;
    };
    let declared = head
        .lines()
        .filter_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(|value| value.trim().parse::<usize>().unwrap_or(0))
        })
        .next()
        .unwrap_or(0);
    body.len() >= declared
}
