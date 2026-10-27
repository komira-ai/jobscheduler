//! `Switch` when a connection ends by itself. The in-file tests cut connections; none lets
//! one finish, so the end of the forwarding task was never reached. Added with the move to
//! this repository.

use std::net::SocketAddr;
use std::time::Duration;

use js_testkit::switch::Switch;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// A server on loopback that answers every connection with `pong` and closes it.
async fn server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("address");
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.expect("accept");
            tokio::spawn(async move {
                let mut ping = [0; 4];
                stream.read_exact(&mut ping).await.expect("ping");
                stream.write_all(b"pong").await.expect("pong");
            });
        }
    });
    addr
}

/// Catches: a forwarder that does not carry a finished connection to its end: the client
/// gets the server's answer and then end of stream when the server closes.
#[tokio::test]
async fn a_connection_that_finishes_ends_cleanly_through_the_switch() {
    let switch = Switch::start(server().await).await;
    let mut client = TcpStream::connect(switch.addr).await.expect("connect");
    client.write_all(b"ping").await.expect("write");
    let mut answer = Vec::new();
    client
        .read_to_end(&mut answer)
        .await
        .expect("read to the end");
    assert_eq!(answer, b"pong");
    drop(client);
    // The forwarding task ends once both sides have closed; let it run before the test does.
    tokio::time::sleep(Duration::from_millis(200)).await;
}
