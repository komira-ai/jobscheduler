//! The proxy in front of tierstore's upstream store: a TCP forwarder that, while held, passes no
//! byte either way and opens no connection to the store, and keeps every connection open, as a
//! store that stopped answering does. Released, it passes on what it held.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::runtime::Handle;
use tokio::sync::watch;

/// The forwarder.
pub struct Stall {
    /// Where tierstore reaches the store.
    pub addr: SocketAddr,
    held: Arc<watch::Sender<bool>>,
}

impl Stall {
    /// A forwarder on a loopback port to `store`, passing bytes, on `runtime`.
    pub fn start(runtime: &Handle, store: SocketAddr) -> Stall {
        let listener = runtime
            .block_on(TcpListener::bind("127.0.0.1:0"))
            .expect("bind the stall proxy");
        let addr = listener.local_addr().expect("its address");
        let held = Arc::new(watch::Sender::new(false));
        let gate = Arc::clone(&held);
        runtime.spawn(async move {
            // A loopback listener's accept fails only when the process is out of descriptors;
            // tierstore's reads of the store then fail and the test with them.
            while let Ok((inbound, _)) = listener.accept().await {
                let gate = Arc::clone(&gate);
                tokio::spawn(async move {
                    open(&gate).await;
                    let Ok(outbound) = TcpStream::connect(store).await else {
                        return;
                    };
                    let (in_read, in_write) = inbound.into_split();
                    let (out_read, out_write) = outbound.into_split();
                    tokio::spawn(pump(in_read, out_write, Arc::clone(&gate)));
                    tokio::spawn(pump(out_read, in_write, gate));
                });
            }
        });
        Stall { addr, held }
    }

    /// From now on nothing passes.
    pub fn hold(&self) {
        self.held.send_replace(true);
    }

    /// From now on everything passes again, what was held first.
    pub fn release(&self) {
        self.held.send_replace(false);
    }
}

/// Returns once the gate is open.
async fn open(gate: &watch::Sender<bool>) {
    let mut gate = gate.subscribe();
    // The sender lives as long as any connection: the wait ends only when the gate opens.
    let _ = gate.wait_for(|held| !*held).await;
}

/// Copies `from` to `to`, each read and each write only while the gate is open.
async fn pump(mut from: OwnedReadHalf, mut to: OwnedWriteHalf, gate: Arc<watch::Sender<bool>>) {
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        open(&gate).await;
        let read = match from.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        open(&gate).await;
        if to.write_all(&buffer[..read]).await.is_err() {
            break;
        }
    }
    let _ = to.shutdown().await;
}
