//! The accepted connections of the server, plain or over TLS.

use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::time::Duration;

use axum::serve::Listener;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;

use crate::util::or_pending;

/// How long a client has to finish its TLS handshake.
pub(crate) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the accept loop waits after an accept fails (out of descriptors, say)
/// before it tries again.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// A byte stream, plain or TLS.
pub(crate) trait Duplex: AsyncRead + AsyncWrite + Send {}

impl<T: AsyncRead + AsyncWrite + Send> Duplex for T {}

/// A connection ready for HTTP.
pub(crate) type Io = Pin<Box<dyn Duplex>>;

/// The listener the HTTP server accepts from. A task accepts the TCP connections and
/// finishes each one's TLS handshake on a task of its own, so a client that stalls its
/// handshake delays nobody; the server receives the connections that got through.
pub(crate) struct Incoming {
    ready: mpsc::Receiver<(Io, SocketAddr)>,
    addr: SocketAddr,
    pump: JoinHandle<()>,
}

impl Incoming {
    /// Starts accepting on `listener`, over TLS when `tls` is set.
    pub(crate) fn new(listener: TcpListener, addr: SocketAddr, tls: Option<TlsAcceptor>) -> Self {
        let (sender, ready) = mpsc::channel(16);
        let pump = tokio::spawn(accept_loop(listener, tls, sender));
        Self::from_parts(ready, addr, pump)
    }

    pub(crate) fn from_parts(
        ready: mpsc::Receiver<(Io, SocketAddr)>,
        addr: SocketAddr,
        pump: JoinHandle<()>,
    ) -> Self {
        Self { ready, addr, pump }
    }
}

impl Drop for Incoming {
    fn drop(&mut self) {
        self.pump.abort();
    }
}

impl Listener for Incoming {
    type Io = Io;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Io, SocketAddr) {
        or_pending(self.ready.recv().await).await
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.addr)
    }
}

async fn accept_loop(
    listener: TcpListener,
    tls: Option<TlsAcceptor>,
    sender: mpsc::Sender<(Io, SocketAddr)>,
) {
    loop {
        let accepted = listener.accept().await;
        let backoff = accepted.is_err().then_some(ACCEPT_BACKOFF);
        accepted.into_iter().for_each(|(stream, peer)| {
            tokio::spawn(handshake(stream, peer, tls.clone(), sender.clone()));
        });
        tokio::time::sleep(backoff.unwrap_or_default()).await;
    }
}

/// Finishes the connection's handshake, if it is a TLS one, and hands it to the server.
/// A client that fails or stalls its handshake is dropped.
async fn handshake(
    stream: TcpStream,
    peer: SocketAddr,
    tls: Option<TlsAcceptor>,
    sender: mpsc::Sender<(Io, SocketAddr)>,
) {
    let io: Option<Io> = match tls {
        None => Some(Box::pin(stream)),
        Some(acceptor) => {
            let shaken = tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await;
            shaken
                .ok()
                .and_then(Result::ok)
                .map(|tls| Box::pin(tls) as Io)
        }
    };
    if let Some(io) = io {
        let _ = sender.send((io, peer)).await;
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::task::{Context, Poll, Waker};

    use super::*;

    /// Asserts that a listener whose accept task has ended stays pending, so the server
    /// goes on until it is told to stop instead of ending or spinning.
    #[test]
    fn accept_stays_pending_without_a_pump() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (sender, ready) = mpsc::channel(1);
            drop(sender);
            let addr: SocketAddr = ([127, 0, 0, 1], 1).into();
            let mut incoming =
                Incoming::from_parts(ready, addr, tokio::spawn(std::future::ready(())));
            assert_eq!(incoming.local_addr().unwrap(), addr);
            let mut accept = std::pin::pin!(incoming.accept());
            let mut cx = Context::from_waker(Waker::noop());
            assert!(matches!(accept.as_mut().poll(&mut cx), Poll::Pending));
        });
    }
}
