//! The accepted connections of the server, plain or over TLS, and the limits on what a
//! client may hold open.
//!
//! One loop accepts and one task serves each connection, from its TLS handshake to its
//! last response. A client that stalls its handshake delays nobody. Every connection
//! holds a permit of a counted gate from before it is accepted until it ends, so the
//! number of connections (handshaking, idle or busy) never passes
//! [`Limits::max_connections`]; at the cap the loop stops accepting and the kernel
//! queues the rest. A client that opens a connection and sends no request head is
//! dropped after [`Limits::header_timeout`], one that stalls its handshake after
//! [`Limits::handshake_timeout`].
//!
//! A permit is held for the connection's whole life, so what a client can do with a
//! connection is bounded in these ways:
//!
//! - the request head must arrive within [`Limits::header_timeout`] of the connection's
//!   start and of the end of each response, which also closes an idle keep-alive
//!   connection;
//! - a read that makes no progress for [`Limits::read_timeout`] closes the connection, so
//!   a client that sends its head (with a valid token) and stops in the body is let go;
//!   the read limit is above the head limit, so it acts in the middle of a request;
//! - a write that makes no progress for [`Limits::write_timeout`] closes the connection,
//!   so a client that sends requests and never reads the answers is let go;
//! - the answer to the [`Limits::max_requests`]th request says `Connection: close`, and
//!   requests pipelined behind it are not answered;
//! - hyper's buffer is [`MAX_BUF`] and a body is at most 64 KiB, so a client cannot make
//!   the server hold more than that for a request.
//!
//! The read and write limits are limits on stalls, not on total time: a client that goes
//! on making progress (a byte in each read period, say) is not cut by them, and what it
//! can send is bounded by the sizes above and by `max_requests`.

use std::future::Future;
use std::io;
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::HeaderValue;
use axum::http::header::CONNECTION;
use axum::middleware::{Next, from_fn_with_state};
use axum::response::Response;
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::service::TowerToHyperService;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot, watch};
use tokio_rustls::TlsAcceptor;
use tracing::warn;

use crate::config::Limits;
use crate::io_timeout::IoTimeout;
use crate::util::or_pending;

/// The most bytes hyper buffers for one connection when it reads: a head larger than
/// this is refused (431) before any route or token sees it.
pub(crate) const MAX_BUF: usize = 64 * 1024;

/// How long the accept loop waits after an accept fails (out of descriptors, say)
/// before it tries again.
pub(crate) const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// A byte stream, plain or TLS.
pub(crate) trait Duplex: AsyncRead + AsyncWrite + Send {}

impl<T: AsyncRead + AsyncWrite + Send> Duplex for T {}

/// A connection ready for HTTP.
pub(crate) type Io = Pin<Box<dyn Duplex>>;

/// Where connections come from: a listening socket, or a test's stand-in for one.
pub(crate) trait Accept {
    /// The next connection, or why there is none.
    fn accept(&self) -> impl Future<Output = io::Result<TcpStream>> + Send;
}

impl Accept for TcpListener {
    async fn accept(&self) -> io::Result<TcpStream> {
        TcpListener::accept(self).await.map(|(stream, _)| stream)
    }
}

/// The next connection of `source`. A failed accept (descriptors used up, say) is
/// logged and retried after `backoff`, so it neither ends the server nor spins.
pub(crate) async fn accept_stream<A: Accept>(source: &A, backoff: Duration) -> TcpStream {
    loop {
        match source.accept().await {
            Ok(stream) => return stream,
            Err(err) => {
                warn!(error = %err, "cannot accept a connection");
                tokio::time::sleep(backoff).await;
            }
        }
    }
}

/// The connection slots: one permit of a counted gate for each connection that may be
/// open, and how often the accept loop found none free and had to wait.
pub(crate) struct Slots {
    gate: Arc<Semaphore>,
    waits: AtomicU64,
}

impl Slots {
    /// `max` slots, all free.
    pub(crate) fn new(max: u16) -> Self {
        Self {
            gate: Arc::new(Semaphore::new(usize::from(max))),
            waits: AtomicU64::new(0),
        }
    }

    /// How many times the accept loop found every slot taken and waited for one. A
    /// loop that waits is not accepting, which is the cap holding.
    pub(crate) fn waits(&self) -> u64 {
        self.waits.load(Ordering::SeqCst)
    }
}

/// The next connection of `listener` and the permit that counts it. Waiting for the
/// permit first is what stops the accepting at the cap.
async fn accept_counted(
    listener: &TcpListener,
    slots: &Slots,
) -> (TcpStream, OwnedSemaphorePermit) {
    let permit = match slots.gate.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            slots.waits.fetch_add(1, Ordering::SeqCst);
            or_pending(slots.gate.clone().acquire_owned().await.ok()).await
        }
    };
    (accept_stream(listener, ACCEPT_BACKOFF).await, permit)
}

/// Serves `app` on `listener` until `stop` fires (or its sender is dropped), then
/// drains: no new connections, each connection finishes the request it is serving, and
/// the connections still open after `limits.drain_timeout` are cut, and the cut ones are
/// dropped before this returns. `slots` holds
/// `limits.max_connections` slots.
pub(crate) async fn serve(
    listener: TcpListener,
    tls: Option<TlsAcceptor>,
    app: Router,
    limits: Limits,
    slots: Arc<Slots>,
    mut stop: oneshot::Receiver<()>,
) {
    let (graceful, _) = watch::channel(());
    let (cut, _) = watch::channel(());
    loop {
        let next = tokio::select! {
            _ = &mut stop => None,
            accepted = accept_counted(&listener, &slots) => Some(accepted),
        };
        let Some((stream, permit)) = next else { break };
        let task = connection(
            stream,
            tls.clone(),
            app.clone(),
            limits,
            graceful.subscribe(),
        );
        let mut cut = cut.subscribe();
        tokio::spawn(async move {
            tokio::select! {
                () = task => {}
                _ = cut.changed() => {}
            }
            drop(permit);
        });
    }
    drop(listener);
    let _ = graceful.send(());
    let all = u32::from(limits.max_connections);
    let drained = tokio::time::timeout(limits.drain_timeout, slots.gate.acquire_many(all)).await;
    if drained.is_err() {
        warn!("connections still open at the end of the drain are cut");
        let _ = cut.send(());
        // A cut task drops its connection (and the router clone in it) when it next runs,
        // and gives its permit back after that: with every permit back, nothing of the
        // server is held by a connection, and a restart finds the lease file free.
        let _ = slots.gate.acquire_many(all).await;
    }
}

/// One connection: its handshake, then its requests, until it ends or `stopping`
/// fires and the request in flight is answered.
async fn connection(
    stream: TcpStream,
    tls: Option<TlsAcceptor>,
    app: Router,
    limits: Limits,
    stopping: watch::Receiver<()>,
) {
    let io = handshake(stream, tls, limits.handshake_timeout).await;
    if let Some(io) = io {
        talk(io, app, limits, stopping).await;
    }
}

/// Finishes the connection's handshake, if it is a TLS one. A client that fails or
/// stalls its handshake gets `None`.
async fn handshake(stream: TcpStream, tls: Option<TlsAcceptor>, limit: Duration) -> Option<Io> {
    match tls {
        None => Some(Box::pin(stream)),
        Some(acceptor) => {
            let shaken = tokio::time::timeout(limit, acceptor.accept(stream)).await;
            shaken
                .ok()
                .and_then(Result::ok)
                .map(|tls| Box::pin(tls) as Io)
        }
    }
}

/// Speaks HTTP/1.1 on `io` until the client is done. When `stopping` fires the request
/// in flight is answered (with `Connection: close`) and an idle connection is closed.
/// An error on the connection (a reset, a head that did not arrive in time, a write
/// that stalled) ends it.
async fn talk(io: Io, app: Router, limits: Limits, mut stopping: watch::Receiver<()>) {
    let io = IoTimeout::new(io, limits.read_timeout, limits.write_timeout);
    let served = Arc::new(Served {
        count: AtomicU32::new(0),
        limit: limits.max_requests,
    });
    let app = app.layer(from_fn_with_state(served, close_after_last));
    let served = http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(limits.header_timeout)
        .max_buf_size(MAX_BUF)
        .serve_connection(TokioIo::new(io), TowerToHyperService::new(app));
    let mut served = pin!(served);
    tokio::select! {
        _ = served.as_mut() => {}
        _ = stopping.changed() => {
            served.as_mut().graceful_shutdown();
            let _ = served.await;
        }
    }
}

/// How many requests a connection has had and how many it may have.
struct Served {
    count: AtomicU32,
    limit: u32,
}

/// Counts the request and, when it is the last the connection may have, makes its
/// answer close the connection.
async fn close_after_last(
    State(served): State<Arc<Served>>,
    request: Request,
    next: Next,
) -> Response {
    let mut response = next.run(request).await;
    let nth = served
        .count
        .fetch_add(1, Ordering::SeqCst)
        .saturating_add(1);
    if nth >= served.limit {
        response
            .headers_mut()
            .insert(CONNECTION, HeaderValue::from_static("close"));
    }
    response
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll, Waker};

    use super::*;

    /// A source whose first accepts fail, then hands out connections of a real listener.
    struct Flaky {
        failures: AtomicUsize,
        listener: TcpListener,
    }

    impl Accept for Flaky {
        async fn accept(&self) -> io::Result<TcpStream> {
            let left = self.failures.load(Ordering::SeqCst);
            match left {
                0 => Accept::accept(&self.listener).await,
                _ => {
                    self.failures.store(left - 1, Ordering::SeqCst);
                    Err(io::Error::other("out of descriptors"))
                }
            }
        }
    }

    async fn listening() -> (TcpListener, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        (listener, client)
    }

    /// Asserts that a failed accept is retried, not fatal: after two failures the
    /// connection that was waiting is returned, and the failures were consumed.
    #[tokio::test]
    async fn a_failed_accept_is_retried() {
        let (listener, client) = listening().await;
        let source = Flaky {
            failures: AtomicUsize::new(2),
            listener,
        };
        let stream = accept_stream(&source, Duration::ZERO).await;
        assert_eq!(source.failures.load(Ordering::SeqCst), 0);
        assert_eq!(stream.peer_addr().unwrap(), client.local_addr().unwrap());
    }

    /// Asserts that the accept loop takes a slot for each connection and gives it back
    /// when the connection ends, that with no slot free it does not accept (the future
    /// stays pending even though a client is waiting) and counts that it waited, and
    /// that taking a free slot is not counted as a wait.
    #[tokio::test]
    async fn a_connection_holds_a_slot_and_no_slot_means_no_accept() {
        let (listener, _client) = listening().await;
        let slots = Slots::new(1);
        let full = slots.gate.clone().acquire_owned().await.unwrap();

        let mut refused = Box::pin(accept_counted(&listener, &slots));
        let mut cx = Context::from_waker(Waker::noop());
        assert!(matches!(refused.as_mut().poll(&mut cx), Poll::Pending));
        assert_eq!(slots.waits(), 1);
        drop(refused);
        drop(full);

        let (stream, permit) = accept_counted(&listener, &slots).await;
        assert_eq!(slots.gate.available_permits(), 0);
        assert_eq!(slots.waits(), 1);
        drop((stream, permit));
        assert_eq!(slots.gate.available_permits(), 1);
    }

    /// What the handler of the next test holds: the signal that it was entered. The
    /// test watches the state through a `Weak` to see when the last router clone and
    /// handler future that hold it are gone.
    struct Held {
        entered: Arc<tokio::sync::Notify>,
    }

    /// A handler that is entered and never answers.
    async fn hold(State(held): State<Arc<Held>>) {
        held.entered.notify_one();
        std::future::pending::<()>().await
    }

    /// Asserts that `serve` returns only after the connections it cut have been dropped:
    /// a request held in its handler past a drain timeout of zero is cut, and when
    /// `serve` returns nothing of the router state is alive any more. The test awaits
    /// `serve` in its own task on a one-thread runtime and looks at the state at once,
    /// so a cut task that is only woken, not yet run, is seen as alive. It catches a
    /// `serve` that returns while cut connection tasks still hold the router, which
    /// lets a restart find the lease file still locked.
    #[tokio::test]
    async fn serve_returns_after_the_cut_connections_are_dropped() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let entered = Arc::new(tokio::sync::Notify::new());
        let held = Arc::new(Held {
            entered: entered.clone(),
        });
        let alive = Arc::downgrade(&held);
        let app = Router::new()
            .route("/", axum::routing::get(hold))
            .with_state(held);
        let limits = Limits {
            drain_timeout: Duration::ZERO,
            ..Limits::default()
        };
        let slots = Arc::new(Slots::new(limits.max_connections));
        let (stop, stopped) = oneshot::channel();
        let client = tokio::spawn(async move {
            let mut client = TcpStream::connect(addr).await.unwrap();
            let head = b"GET / HTTP/1.1\r\nHost: t\r\n\r\n";
            tokio::io::AsyncWriteExt::write_all(&mut client, head)
                .await
                .unwrap();
            entered.notified().await;
            let _ = stop.send(());
            client
        });
        let bound = Duration::from_secs(30);
        let ended = serve(listener, None, app, limits, slots, stopped);
        tokio::time::timeout(bound, ended)
            .await
            .expect("serve did not return");
        assert_eq!(
            alive.strong_count(),
            0,
            "a cut connection still holds the router"
        );
        drop(client.await.unwrap());
    }
}
