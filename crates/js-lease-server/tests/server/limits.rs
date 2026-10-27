//! What a client can hold open: connections, request heads, and the drain at a stop.

use std::time::Duration;

use js_lease_server::Limits;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::support::{
    Gate, Harness, Scratch, SharedPool, TOKEN, base_args, lease_body, parse_reply, request,
};

/// Limits with a short `header_timeout` and `drain_timeout`. Tests that use them only
/// wait for the server to close a stalled connection; a good request is made on a
/// server with the default limits.
fn short() -> Limits {
    Limits {
        header_timeout: Duration::from_millis(20),
        drain_timeout: Duration::from_millis(20),
        ..Limits::default()
    }
}

/// Asserts that a connection that sends part of a request head and stops is closed by
/// the server once `header_timeout` has passed: the client reads the end of the stream
/// and no answer.
#[tokio::test]
async fn a_stalled_request_head_is_closed_at_its_timeout() {
    let h = Harness::limited("limit_header", short()).await;
    let mut stream = TcpStream::connect(h.addr()).await.unwrap();
    stream
        .write_all(b"GET /healthz HTTP/1.1\r\n")
        .await
        .unwrap();
    let mut seen = Vec::new();
    stream.read_to_end(&mut seen).await.unwrap();
    assert_eq!(parse_reply(&seen).status, 0);
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

/// Asserts that the connection cap counts every connection and gives its slot back
/// when it ends: with a cap of one, a run of requests one after another is served, and
/// a connection waiting for the slot is served once the one holding it is done.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cap_of_one_serves_connections_in_turn() {
    let limits = Limits {
        max_connections: 1,
        ..Limits::default()
    };
    let (entered, inside) = std::sync::mpsc::channel();
    let (release, go) = std::sync::mpsc::channel();
    let pool = SharedPool::new().gated(Gate { entered, go });
    let scratch = Scratch::new("limit_cap");
    let config = crate::support::config(base_args(&scratch)).unwrap();
    let config = config
        .with_limits(limits)
        .with_capacity_source(Box::new(pool));
    let h = Harness::over(scratch, config).await;
    for _ in 0..3 {
        let health = request(h.addr(), "GET", "/healthz", None, None).await;
        assert_eq!(health.status, 200);
    }

    let addr = h.addr();
    let body = lease_body().to_string().into_bytes();
    let holder =
        tokio::spawn(
            async move { request(addr, "POST", "/v1/leases", Some(TOKEN), Some(&body)).await },
        );
    inside.recv().unwrap();
    let waiting = tokio::spawn(async move { request(addr, "GET", "/healthz", None, None).await });
    release.send(()).unwrap();
    assert_eq!(holder.await.unwrap().status, 201);
    assert_eq!(waiting.await.unwrap().status, 200);
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

/// Asserts that a stop is bounded by the drain timeout: a request stalled in its body
/// is cut, the stop still returns and the last tick still runs (a lease that fell due
/// is gone from the file), and the client sees the connection end with no answer.
#[tokio::test]
async fn a_stalled_request_cannot_hold_up_a_stop() {
    let limits = Limits {
        drain_timeout: Duration::from_millis(20),
        ..Limits::default()
    };
    let h = Harness::limited("limit_drain", limits).await;
    h.call("POST", "/v1/leases", Some(&lease_body())).await;
    h.clock.advance(Duration::from_secs(601));
    let mut stalled = stall_in_body(h.addr()).await;
    let state = h.scratch.state();

    h.running.shutdown().await.unwrap();
    let mut seen = Vec::new();
    stalled.read_to_end(&mut seen).await.unwrap();
    assert!(seen.is_empty());
    assert!(
        std::fs::read_to_string(state)
            .unwrap()
            .contains("lease_count = 0")
    );
}
