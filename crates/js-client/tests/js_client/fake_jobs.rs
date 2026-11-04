//! An in-memory job server for the job client's tests: a gRPC service at the raw HTTP/2
//! level that records each request (its call, its `authorization` metadata, the message
//! bytes on the wire) and answers as the test says. Every connection the client dials is a
//! fresh in-memory stream with the server on the other end, so no test opens a port or
//! resolves a name, and nothing waits on a clock.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use http_body_util::BodyExt;
use hyper::body::Frame;
use js_client::jobs::{Dialer, Io};
use tokio::io::{AsyncRead, AsyncWrite, DuplexStream, ReadBuf};
use tokio::task::AbortHandle;
use tonic::body::Body;
use tonic::codegen::{Bytes, Service, http, tokio_stream};
use tonic::transport::Server;
use tonic::transport::server::Connected;
use tonic::{Code, Status};

/// How the fake answers a call.
#[derive(Clone)]
pub enum Answer {
    /// This encoded message, with status OK.
    Message(Vec<u8>),
    /// This status, after reading the request.
    Status(Code, &'static str),
    /// Nothing: once the request is read, every connection of the fake is dropped.
    Hang,
    /// An HTTP redirect to another origin.
    Redirect,
    /// The response headers, and then a body that never ends.
    Stall,
}

/// One request the fake read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seen {
    /// The call's path, such as `/jobscheduler.v1.JobService/SubmitJob`.
    pub path: String,
    /// The request's whole URI, scheme and authority included.
    pub uri: String,
    /// The `authorization` metadata.
    pub authorization: Option<String>,
    /// The message bytes, without gRPC's five-byte frame prefix.
    pub message: Vec<u8>,
}

/// The fake job server.
#[derive(Clone, Default)]
pub struct Fake {
    answers: Arc<Mutex<BTreeMap<String, Vec<Answer>>>>,
    seen: Arc<Mutex<Vec<Seen>>>,
    tasks: Arc<Mutex<Vec<AbortHandle>>>,
    dials: Arc<Mutex<Vec<(String, u16)>>>,
}

impl Fake {
    /// A fake that answers every call with an empty message.
    pub fn new() -> Self {
        Self::default()
    }

    /// Answers the next calls of `method` (`SubmitJob`) with `answers`, in order; the last
    /// one stays.
    pub fn answer(&self, method: &str, answers: Vec<Answer>) -> &Self {
        let path = format!("/jobscheduler.v1.JobService/{method}");
        self.answers.lock().unwrap().insert(path, answers);
        self
    }

    /// The requests read so far.
    pub fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    /// How many connections the client dialled.
    pub fn dials(&self) -> usize {
        self.dials.lock().unwrap().len()
    }

    /// The host and port of each connection the client dialled.
    pub fn destinations(&self) -> Vec<(String, u16)> {
        self.dials.lock().unwrap().clone()
    }

    /// A dialer whose every connection is served by this fake, inside TLS with `tls` when it
    /// is given.
    pub fn dialer(&self, tls: Option<Arc<rustls::ServerConfig>>) -> Dialer {
        let fake = self.clone();
        Dialer::new(move |host, port| {
            let (client, server) = tokio::io::duplex(1 << 16);
            fake.dials.lock().unwrap().push((host.to_owned(), port));
            let task = tokio::spawn(serve(fake.clone(), server, tls.clone()));
            fake.tasks.lock().unwrap().push(task.abort_handle());
            Box::pin(async move { Ok(Box::new(client) as Box<dyn Io>) })
        })
    }

    /// The answer to the next call of `path`.
    fn next_answer(&self, path: &str) -> Answer {
        let mut answers = self.answers.lock().unwrap();
        let queue = answers.entry(path.to_owned()).or_default();
        match queue.len() {
            0 => Answer::Message(Vec::new()),
            1 => queue[0].clone(),
            _ => queue.remove(0),
        }
    }
}

/// A dialer that fails before anything is written, as a refused connection does.
pub fn refusing() -> Dialer {
    Dialer::new(|_host, _port| {
        Box::pin(async {
            Err(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                "nothing listens",
            ))
        })
    })
}

/// Serves one connection.
async fn serve(fake: Fake, stream: DuplexStream, tls: Option<Arc<rustls::ServerConfig>>) {
    match tls {
        None => {
            let incoming = tokio_stream::once(Ok::<_, io::Error>(stream));
            let _ = Server::builder().serve_with_incoming(fake, incoming).await;
        }
        Some(config) => {
            let Ok(stream) = tokio_rustls::TlsAcceptor::from(config).accept(stream).await else {
                return;
            };
            let incoming = tokio_stream::once(Ok::<_, io::Error>(Tls(stream)));
            let _ = Server::builder().serve_with_incoming(fake, incoming).await;
        }
    }
}

impl Service<http::Request<Body>> for Fake {
    type Response = http::Response<Body>;
    type Error = Infallible;
    type Future =
        Pin<Box<dyn std::future::Future<Output = Result<Self::Response, Infallible>> + Send>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: http::Request<Body>) -> Self::Future {
        let fake = self.clone();
        Box::pin(async move {
            let path = request.uri().path().to_owned();
            let uri = request.uri().to_string();
            let authorization = request
                .headers()
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            let body = request
                .into_body()
                .collect()
                .await
                .map(|b| b.to_bytes())
                .unwrap_or_default();
            let message = body.get(5..).unwrap_or_default().to_vec();
            fake.seen.lock().unwrap().push(Seen {
                path: path.clone(),
                uri,
                authorization,
                message,
            });
            Ok(match fake.next_answer(&path) {
                Answer::Message(bytes) => ok(bytes),
                Answer::Status(code, message) => Status::new(code, message).into_http(),
                Answer::Redirect => http::Response::builder()
                    .status(307)
                    .header("location", "https://elsewhere.example/")
                    .body(Body::empty())
                    .unwrap(),
                Answer::Stall => {
                    let never = tokio_stream::pending::<Result<Frame<Bytes>, Status>>();
                    http::Response::builder()
                        .header("content-type", "application/grpc")
                        .body(Body::new(http_body_util::StreamBody::new(never)))
                        .unwrap()
                }
                Answer::Hang => {
                    fake.tasks
                        .lock()
                        .unwrap()
                        .iter()
                        .for_each(AbortHandle::abort);
                    std::future::pending::<()>().await;
                    unreachable!("a pending future never ends")
                }
            })
        })
    }
}

/// A gRPC answer: `message` in one frame, then the trailers with status OK.
fn ok(message: Vec<u8>) -> http::Response<Body> {
    let mut frame = vec![0u8];
    frame.extend(u32::try_from(message.len()).unwrap().to_be_bytes());
    frame.extend(message);
    let mut trailers = http::HeaderMap::new();
    trailers.insert("grpc-status", http::HeaderValue::from_static("0"));
    let frames = tokio_stream::iter([
        Ok::<_, Status>(Frame::data(Bytes::from(frame))),
        Ok(Frame::trailers(trailers)),
    ]);
    http::Response::builder()
        .header("content-type", "application/grpc")
        .body(Body::new(http_body_util::StreamBody::new(frames)))
        .unwrap()
}

/// A TLS stream the gRPC server can serve.
struct Tls(tokio_rustls::server::TlsStream<DuplexStream>);

impl Connected for Tls {
    type ConnectInfo = ();

    fn connect_info(&self) -> Self::ConnectInfo {}
}

impl AsyncRead for Tls {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl AsyncWrite for Tls {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

/// A server TLS configuration for a self-signed certificate of `jobs.example`, offering
/// HTTP/2; and the certificate in PEM.
pub fn tls_server() -> (Arc<rustls::ServerConfig>, String) {
    use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
    let made = rcgen::generate_simple_self_signed(vec!["jobs.example".to_owned()]).unwrap();
    let key = PrivateKeyDer::from(PrivatePkcs8KeyDer::from(made.signing_key.serialize_der()));
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![made.cert.der().clone()], key)
        .unwrap();
    config.alpn_protocols = vec![b"h2".to_vec()];
    (Arc::new(config), made.cert.pem())
}

/// A dialer that never opens its stream.
pub fn stalling() -> Dialer {
    Dialer::new(|_host, _port| Box::pin(std::future::pending()))
}
