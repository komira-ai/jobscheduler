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
//!
//! # HTTP/1.1, or both
//!
//! The lease and agent listeners ([`serve`]) speak HTTP/1.1 alone. The job service's
//! listener ([`serve_both`]) speaks HTTP/1.1 or HTTP/2 on the same port, whichever the
//! client starts with (gRPC is HTTP/2), with the same accept loop, slots, timeouts and
//! drain. An HTTP/1.1 connection there is held to everything above. An HTTP/2 connection
//! is held to the same handshake, read and write limits; its head is at most [`MAX_BUF`];
//! `max_requests` and the `Connection: close` it adds do not apply to it (HTTP/2 has no
//! such header, and its streams are bounded by the protocol's concurrency limit instead).
//! HTTP/2 runs each request on a task of its own: those tasks hold the connection's slot
//! and are cut with it, so a drain that cuts a connection cuts its requests too, and
//! nothing a connection started outlives [`serve_both`].

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use axum::Extension;
use axum::Router;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::HeaderValue;
use axum::http::header::CONNECTION;
use axum::middleware::{Next, from_fn_with_state};
use axum::response::Response;
use hyper::rt::Executor;
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
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

/// What the connections of a listener speak.
#[derive(Debug, Clone, Copy)]
enum Speech {
    /// HTTP/1.1 alone: the lease and agent listeners.
    Http1,
    /// HTTP/1.1 or HTTP/2, as the client starts: the job service's listener.
    Both,
}

/// Serves `app` on `listener` over HTTP/1.1 until `stop` fires (or its sender is dropped),
/// then drains: no new connections, each connection finishes the request it is serving,
/// and the connections still open after `limits.drain_timeout` are cut, and the cut ones
/// are dropped before this returns. `slots` holds `limits.max_connections` slots.
pub(crate) async fn serve(
    listener: TcpListener,
    tls: Option<TlsAcceptor>,
    app: Router,
    limits: Limits,
    slots: Arc<Slots>,
    stop: oneshot::Receiver<()>,
) {
    accept_all(listener, tls, app, limits, slots, stop, Speech::Http1).await;
}

/// [`serve`], speaking HTTP/1.1 or HTTP/2 on each connection, as its client starts: the job
/// service's listener. A request of an HTTP/2 connection that is cut is cut too, and is
/// dropped before this returns.
pub(crate) async fn serve_both(
    listener: TcpListener,
    tls: Option<TlsAcceptor>,
    app: Router,
    limits: Limits,
    slots: Arc<Slots>,
    stop: oneshot::Receiver<()>,
) {
    accept_all(listener, tls, app, limits, slots, stop, Speech::Both).await;
}

/// The accept loop and the drain of [`serve`] and [`serve_both`].
async fn accept_all(
    listener: TcpListener,
    tls: Option<TlsAcceptor>,
    app: Router,
    limits: Limits,
    slots: Arc<Slots>,
    mut stop: oneshot::Receiver<()>,
    speech: Speech,
) {
    let (graceful, _) = watch::channel(());
    let (cut, _) = watch::channel(());
    loop {
        let next = tokio::select! {
            _ = &mut stop => None,
            accepted = accept_counted(&listener, &slots) => Some(accepted),
        };
        let Some((stream, permit)) = next else { break };
        let permit = Arc::new(permit);
        let streams = Streams {
            permit: Arc::clone(&permit),
            cut: cut.subscribe(),
        };
        let talk = Talk {
            app: app.clone(),
            limits,
            stopping: graceful.subscribe(),
            speech,
            streams,
        };
        let task = connection(stream, tls.clone(), talk);
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
        // and gives its share of the permit back after that, as does each HTTP/2 request
        // task of the connection: with every permit back, nothing of the server is held by a
        // connection, and a restart finds the lease file (and the job store) free.
        let _ = slots.gate.acquire_many(all).await;
    }
}

/// What a connection's requests are served with once its handshake is done.
struct Talk {
    app: Router,
    limits: Limits,
    /// Fires when the listener stops: the request in flight is answered, then the
    /// connection ends.
    stopping: watch::Receiver<()>,
    speech: Speech,
    /// Where an HTTP/2 connection runs its requests.
    streams: Streams,
}

/// One connection: its handshake, then its requests, until it ends or `stopping`
/// fires and the request in flight is answered.
async fn connection(stream: TcpStream, tls: Option<TlsAcceptor>, talk: Talk) {
    let Talk {
        app,
        limits,
        stopping,
        speech,
        streams,
    } = talk;
    // The peer is read before the handshake, so a plain and a TLS connection carry it the
    // same way. A stream that cannot name its peer carries none, and the routes that
    // gate on the peer refuse it.
    let app = with_peer(app, stream.peer_addr().ok());
    let io = handshake(stream, tls, limits.handshake_timeout).await;
    if let Some(io) = io {
        match speech {
            Speech::Http1 => talk_http1(io, app, limits, stopping).await,
            Speech::Both => talk_both(io, app, limits, stopping, streams).await,
        }
    }
}

/// `app` with `peer` as the [`ConnectInfo`] every request of the connection carries; with
/// no peer it carries none.
fn with_peer(app: Router, peer: Option<SocketAddr>) -> Router {
    peer.into_iter()
        .fold(app, |app, peer| app.layer(Extension(ConnectInfo(peer))))
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
async fn talk_http1(io: Io, app: Router, limits: Limits, mut stopping: watch::Receiver<()>) {
    let io = IoTimeout::new(io, limits.read_timeout, limits.write_timeout);
    let app = counted(app, limits);
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

/// Speaks HTTP/1.1 or HTTP/2 on `io`, as the client starts, until the client is done.
/// HTTP/1.1 is served as [`talk_http1`] serves it; an HTTP/2 connection runs its requests
/// on `streams`. When `stopping` fires the requests in flight are answered and the
/// connection is closed (HTTP/2 tells the client to open no new request first).
async fn talk_both(
    io: Io,
    app: Router,
    limits: Limits,
    mut stopping: watch::Receiver<()>,
    streams: Streams,
) {
    let io = IoTimeout::new(io, limits.read_timeout, limits.write_timeout);
    let app = counted(app, limits);
    let mut builder = auto::Builder::new(streams);
    builder
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(limits.header_timeout)
        .max_buf_size(MAX_BUF);
    builder
        .http2()
        .timer(TokioTimer::new())
        .max_header_list_size(MAX_HEAD_LIST);
    let served = builder.serve_connection(TokioIo::new(io), TowerToHyperService::new(app));
    let mut served = pin!(served);
    tokio::select! {
        _ = served.as_mut() => {}
        _ = stopping.changed() => {
            served.as_mut().graceful_shutdown();
            let _ = served.await;
        }
    }
}

/// The most bytes of headers an HTTP/2 request may carry: [`MAX_BUF`], the limit an
/// HTTP/1.1 head is held to.
const MAX_HEAD_LIST: u32 = 64 * 1024;

/// Where an HTTP/2 connection runs its requests: each on a task of its own, as hyper wants,
/// holding the connection's slot until it ends and ended by the listener's cut, so that the
/// slots count every task a connection started and a drain that cuts the connection cuts
/// its requests too.
#[derive(Clone)]
struct Streams {
    permit: Arc<OwnedSemaphorePermit>,
    cut: watch::Receiver<()>,
}

impl<F> Executor<F> for Streams
where
    F: Future<Output = ()> + Send + 'static,
{
    fn execute(&self, stream: F) {
        let permit = Arc::clone(&self.permit);
        let mut cut = self.cut.clone();
        tokio::spawn(async move {
            tokio::select! {
                () = stream => {}
                _ = cut.changed() => {}
            }
            drop(permit);
        });
    }
}

/// `app`, counting the requests of one connection: the answer to the `max_requests`th
/// closes an HTTP/1.1 connection.
fn counted(app: Router, limits: Limits) -> Router {
    let served = Arc::new(Served {
        count: AtomicU32::new(0),
        limit: limits.max_requests,
    });
    app.layer(from_fn_with_state(served, close_after_last))
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

    /// A handler that is entered and never answers. It is not an `async fn`, whose
    /// closing line after the pending future could never run.
    fn hold(State(held): State<Arc<Held>>) -> impl Future<Output = ()> {
        held.entered.notify_one();
        std::future::pending()
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

    /// Asserts that `serve_both` cuts the HTTP/2 requests of a connection it cuts, and
    /// returns only after they are dropped: a gRPC call (HTTP/2, prior knowledge) held in
    /// its handler past a drain timeout of zero is cut, and when `serve_both` returns
    /// nothing of the router state is alive, every slot is free, and the call has failed.
    /// A request of an HTTP/2 connection runs on a task of its own; it catches such a task
    /// that outlives the drain (holding the router, and with it the job store's file, which
    /// a restart could then not open) or that holds no slot.
    #[tokio::test]
    async fn serve_both_returns_after_the_cut_http2_requests_are_dropped() {
        use tonic::client::Grpc;
        use tonic::codegen::http::uri::PathAndQuery;
        use tonic::transport::Endpoint;
        use tonic_prost::ProstCodec;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let entered = Arc::new(tokio::sync::Notify::new());
        let held = Arc::new(Held {
            entered: entered.clone(),
        });
        let alive = Arc::downgrade(&held);
        let app = Router::new()
            .route("/hold", axum::routing::post(hold))
            .with_state(held);
        let limits = Limits {
            drain_timeout: Duration::ZERO,
            ..Limits::default()
        };
        let slots = Arc::new(Slots::new(limits.max_connections));
        let (stop, stopped) = oneshot::channel();
        let call = tokio::spawn(async move {
            let channel = Endpoint::from_shared(format!("http://{addr}"))
                .unwrap()
                .connect()
                .await
                .unwrap();
            let mut grpc = Grpc::new(channel);
            grpc.ready().await.unwrap();
            let path = PathAndQuery::from_static("/hold");
            let request = tonic::Request::new(());
            grpc.unary::<(), (), _>(request, path, ProstCodec::default())
                .await
        });
        let stopper = tokio::spawn(async move {
            entered.notified().await;
            let _ = stop.send(());
        });
        let bound = Duration::from_secs(30);
        let ended = serve_both(listener, None, app, limits, slots.clone(), stopped);
        tokio::time::timeout(bound, ended)
            .await
            .expect("serve_both did not return");
        assert_eq!(
            alive.strong_count(),
            0,
            "a cut request still holds the router"
        );
        assert_eq!(
            slots.gate.available_permits(),
            usize::from(limits.max_connections)
        );
        stopper.await.unwrap();
        assert!(call.await.unwrap().is_err());
    }
}

#[cfg(test)]
#[path = "listener_answer_tests.rs"]
mod answer_tests;
