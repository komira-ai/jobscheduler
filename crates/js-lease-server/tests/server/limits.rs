//! What a client can hold open: connections, request heads, writes that are not read,
//! requests per connection, and the drain at a stop.
//!
//! A test about a timeout sets that one timeout small through [`Limits`] and nothing
//! else, and only waits for the server to close a connection; it never sleeps and never
//! asserts how long anything took. The few requests such a test needs besides the one
//! the timeout acts on go to a server with the default limits or are sent after the
//! connection under test has been seen to end.

use std::io;
use std::time::Duration;

use js_lease_server::Limits;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpSocket, TcpStream};

use crate::support::{Harness, TOKEN, guard, lease_body, parse_reply, request, until};

/// One request for the health route, kept alive.
const HEALTH: &str = "GET /healthz HTTP/1.1\r\nHost: t\r\n\r\n";

/// Asserts that a connection that sends part of a request head and stops is closed by
/// the server once `header_timeout` has passed: the client reads the end of the stream
/// and no answer.
#[tokio::test]
async fn a_stalled_request_head_is_closed_at_its_timeout() {
    let limits = Limits {
        header_timeout: Duration::from_millis(20),
        ..Limits::default()
    };
    let h = Harness::limited("limit_header", limits).await;
    let mut stream = TcpStream::connect(h.addr()).await.unwrap();
    stream
        .write_all(b"GET /healthz HTTP/1.1\r\n")
        .await
        .unwrap();
    let mut seen = Vec::new();
    guard(stream.read_to_end(&mut seen)).await.unwrap();
    assert_eq!(parse_reply(&seen).status, 0);
}

/// Asserts that a connection that sends nothing at all is closed by the server once
/// `header_timeout` has passed: the client reads the end of the stream and no bytes.
#[tokio::test]
async fn a_connection_that_sends_nothing_is_closed_at_the_header_timeout() {
    let limits = Limits {
        header_timeout: Duration::from_millis(20),
        ..Limits::default()
    };
    let h = Harness::limited("limit_nothing", limits).await;
    let mut stream = TcpStream::connect(h.addr()).await.unwrap();
    let mut seen = Vec::new();
    guard(stream.read_to_end(&mut seen)).await.unwrap();
    assert!(seen.is_empty());
}

/// Asserts that a keep-alive connection that has had its answer and then sends nothing
/// is closed by the server when the header clock, which starts again after each
/// response, runs out: the client reads the whole first answer, which did not say
/// `Connection: close`, and then the end of the stream. The header timeout is short
/// (200 ms); the request head is written as soon as the connection is made, so it is
/// already waiting when the server's clock starts, and the client drives nothing by
/// time: it reads the whole answer and then waits for the end of the stream.
#[tokio::test]
async fn an_idle_keep_alive_connection_is_closed_after_its_answer() {
    let limits = Limits {
        header_timeout: Duration::from_millis(200),
        ..Limits::default()
    };
    let h = Harness::limited("limit_idle", limits).await;
    let mut stream = TcpStream::connect(h.addr()).await.unwrap();
    stream.write_all(HEALTH.as_bytes()).await.unwrap();
    let mut seen = Vec::new();
    guard(stream.read_to_end(&mut seen)).await.unwrap();
    let text = String::from_utf8_lossy(&seen).to_lowercase();
    assert!(text.starts_with("http/1.1 200"), "{text}");
    assert!(text.ends_with("\r\n\r\nok"), "{text}");
    assert!(!text.contains("connection: close"), "{text}");
}

/// Asserts that a client that sends a valid authenticated head with a body longer than
/// it then sends, and stops, is cut by `read_timeout` and its slot returned: with a cap
/// of one, the client sees the end of the stream (after at most a 400 for the body that
/// did not arrive) and a second client is
/// served although the first never closed. (The head is sent in one write with the ten
/// bytes of body; a machine so loaded that the timeout ran out before they arrived
/// cuts the connection earlier, which is the same observation.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_that_stops_in_the_body_is_cut_and_its_slot_returned() {
    let limits = Limits {
        max_connections: 1,
        read_timeout: Duration::from_millis(20),
        ..Limits::default()
    };
    let h = Harness::limited("limit_read", limits).await;
    let mut stalled = TcpStream::connect(h.addr()).await.unwrap();
    let head = format!(
        "POST /v1/leases HTTP/1.1\r\nHost: t\r\nAuthorization: Bearer {TOKEN}\r\nContent-Length: 100\r\n\r\n0123456789"
    );
    let _ = stalled.write_all(head.as_bytes()).await;

    let mut seen = Vec::new();
    let _ = guard(stalled.read_to_end(&mut seen)).await;
    let status = parse_reply(&seen).status;
    assert!(matches!(status, 0 | 400), "{status}");
    let served = guard(request(h.addr(), "GET", "/healthz", None, None)).await;
    assert_eq!(served.status, 200);
}

/// Asserts that a client that connects and sends nothing does not stop the server
/// answering others while its connection is open (the default limits).
#[tokio::test]
async fn a_silent_connection_delays_nobody() {
    let h = Harness::new("limit_silent").await;
    let silent = TcpStream::connect(h.addr()).await.unwrap();
    let health = request(h.addr(), "GET", "/healthz", None, None).await;
    assert_eq!(health.status, 200);
    assert!(silent.peer_addr().is_ok());
}

/// Asserts that the connection cap binds and gives its slot back when a connection
/// ends: with a cap of one, once a connection is open the accept loop is parked waiting
/// for a slot (an event a server without the cap never produces), and a request on a
/// second connection is served once the first has gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cap_of_one_serves_connections_in_turn() {
    let limits = Limits {
        max_connections: 1,
        ..Limits::default()
    };
    let h = Harness::limited("limit_cap", limits).await;
    let first = TcpStream::connect(h.addr()).await.unwrap();
    until(|| h.running.accept_waits() == 1).await;

    let addr = h.addr();
    let second = tokio::spawn(async move { request(addr, "GET", "/healthz", None, None).await });
    drop(first);
    assert_eq!(guard(second).await.unwrap().status, 200);
}

/// Asserts that a client that sends requests and never reads the answers is cut once a
/// write to it has made no progress for `write_timeout`, and that its slot is given
/// back: with a cap of one, a second client is served although the first never closed.
/// The first client's own write fails when the server has closed it. Its receive buffer
/// is tiny, so the server's answers back up after a few hundred requests. The request
/// cap is lifted so that only the write timeout can end the connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_that_never_reads_is_cut_and_its_slot_returned() {
    let limits = Limits {
        max_connections: 1,
        write_timeout: Duration::from_millis(20),
        max_requests: u32::MAX,
        ..Limits::default()
    };
    let h = Harness::limited("limit_write", limits).await;
    let socket = TcpSocket::new_v4().unwrap();
    socket.set_recv_buffer_size(1024).unwrap();
    let stream = socket.connect(h.addr()).await.unwrap();
    let (_unread, mut sending) = stream.into_split();
    let flood = tokio::spawn(async move {
        let batch = HEALTH.repeat(200);
        loop {
            if let Err(err) = sending.write_all(batch.as_bytes()).await {
                return err;
            }
        }
    });

    let served = guard(request(h.addr(), "GET", "/healthz", None, None)).await;
    assert_eq!(served.status, 200);
    let cut = guard(flood).await.unwrap();
    assert!(
        matches!(
            cut.kind(),
            io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
        ),
        "{cut:?}"
    );
}

/// Asserts that a connection serves `max_requests` requests and the answer to the last
/// says `Connection: close`: of three requests sent at once with a limit of two, the
/// client gets two answers, only the second says close, and the stream ends without a
/// third.
#[tokio::test]
async fn the_last_request_a_connection_may_have_closes_it() {
    let limits = Limits {
        max_requests: 2,
        ..Limits::default()
    };
    let h = Harness::limited("limit_requests", limits).await;
    let mut stream = TcpStream::connect(h.addr()).await.unwrap();
    stream.write_all(HEALTH.repeat(3).as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    guard(stream.read_to_end(&mut raw)).await.unwrap();
    let text = String::from_utf8_lossy(&raw).to_lowercase();
    let answers: Vec<&str> = text.split("http/1.1 ").skip(1).collect();
    assert_eq!(answers.len(), 2, "{text}");
    assert!(!answers[0].contains("connection: close"), "{text}");
    assert!(answers[1].contains("connection: close"), "{text}");
}

/// Asserts that a request head over the buffer the server reads is refused with a 431
/// before the token is looked at, however large the client makes it.
#[tokio::test]
async fn an_oversized_request_head_is_refused() {
    let h = Harness::new("limit_head").await;
    let mut stream = TcpStream::connect(h.addr()).await.unwrap();
    let pad = "x".repeat(200 * 1024);
    let head = format!("GET /v1/leases HTTP/1.1\r\nHost: t\r\nX-Pad: {pad}\r\n\r\n");
    let _ = stream.write_all(head.as_bytes()).await;
    let mut raw = Vec::new();
    let _ = guard(stream.read_to_end(&mut raw)).await;
    assert_eq!(parse_reply(&raw).status, 431);
}

/// Sends the head of an authenticated lease request that expects `100 Continue` and
/// reads that answer: the server has taken the connection and its handler is waiting
/// for the body, which is never sent. Returns the stream.
async fn stall_in_body(addr: std::net::SocketAddr) -> TcpStream {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let head = format!(
        "POST /v1/leases HTTP/1.1\r\nHost: t\r\nAuthorization: Bearer {TOKEN}\r\nExpect: 100-continue\r\nContent-Length: 10\r\n\r\n"
    );
    stream.write_all(head.as_bytes()).await.unwrap();
    let mut line = [0u8; 25];
    stream.read_exact(&mut line).await.unwrap();
    assert_eq!(&line[..], b"HTTP/1.1 100 Continue\r\n\r\n");
    stream
}

/// Asserts that a stop is bounded by the drain timeout (the read timeout is long, so it
/// is not what ends the stall): a request stalled in its body
/// is cut, the stop still returns and the last tick still runs (a lease that fell due
/// is gone from the file), and the client sees the connection end with no answer.
#[tokio::test]
async fn a_stalled_request_cannot_hold_up_a_stop() {
    let limits = Limits {
        drain_timeout: Duration::from_millis(20),
        read_timeout: Duration::from_secs(3600),
        ..Limits::default()
    };
    let h = Harness::limited("limit_drain", limits).await;
    h.call("POST", "/v1/leases", Some(&lease_body())).await;
    h.clock.advance(Duration::from_secs(601));
    let mut stalled = stall_in_body(h.addr()).await;
    let state = h.scratch.state();

    h.running.shutdown().await.unwrap();
    let mut seen = Vec::new();
    guard(stalled.read_to_end(&mut seen)).await.unwrap();
    assert!(seen.is_empty());
    assert!(
        std::fs::read_to_string(state)
            .unwrap()
            .contains("lease_count = 0")
    );
}
