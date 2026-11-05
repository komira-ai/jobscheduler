//! An in-memory job server for the job commands' tests (as js-client's tests have one): a gRPC service at the raw HTTP/2
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
use tokio::io::DuplexStream;
use tokio::task::AbortHandle;
use tonic::body::Body;
use tonic::codegen::{Bytes, Service, http, tokio_stream};
use tonic::transport::Server;
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
    /// A refused submit: this refusal, carried in the status details.
    Refused(js_proto::v1::RefusalCode, &'static str),
}

/// One request the fake read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seen {
    /// The call's path, such as `/jobscheduler.v1.JobService/SubmitJob`.
    pub path: String,
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

    /// A dialer whose every connection is served by this fake.
    pub fn dialer(&self) -> Dialer {
        self.dialer_with(|| {})
    }

    /// A dialer whose every connection is served by this fake, calling `on_dial` first.
    pub fn dialer_with(&self, on_dial: impl Fn() + Send + Sync + 'static) -> Dialer {
        let fake = self.clone();
        Dialer::new(move |_host, _port| {
            on_dial();
            let (client, server) = relayed(&fake.tasks);
            let task = tokio::spawn(serve(fake.clone(), server));
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

/// The client's and the server's ends of one connection, joined by a relay task listed in
/// `tasks`: aborting the relay drops both ends, so the client sees the connection close
/// (the server's own tasks, which tonic spawns per connection, would keep it open).
fn relayed(tasks: &Mutex<Vec<AbortHandle>>) -> (DuplexStream, DuplexStream) {
    let (client, mut near) = tokio::io::duplex(1 << 16);
    let (mut far, server) = tokio::io::duplex(1 << 16);
    let relay = tokio::spawn(async move {
        let _ = tokio::io::copy_bidirectional(&mut near, &mut far).await;
    });
    tasks.lock().unwrap().push(relay.abort_handle());
    (client, server)
}

/// Serves one connection.
async fn serve(fake: Fake, stream: DuplexStream) {
    let incoming = tokio_stream::once(Ok::<_, io::Error>(stream));
    let _ = Server::builder().serve_with_incoming(fake, incoming).await;
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
                authorization,
                message,
            });
            Ok(match fake.next_answer(&path) {
                Answer::Message(bytes) => ok(bytes),
                Answer::Status(code, message) => Status::new(code, message).into_http(),
                Answer::Refused(code, detail) => {
                    js_proto::refusal::refusal_status(code, detail).into_http()
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
