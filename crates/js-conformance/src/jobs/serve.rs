//! Serving a gRPC service to a client in the same process.

use std::future::{Ready, ready};
use std::io;
use std::task::{Context, Poll};

use hyper_util::rt::TokioIo;
use tokio::io::DuplexStream;
use tonic::body::Body;
use tonic::codegen::{Bytes, Service, StdError, http, tokio_stream};
use tonic::transport::{Channel, Endpoint, Server};

/// The bytes each direction of the in-memory stream buffers before a write waits.
const BUFFER: usize = 64 * 1024;

/// A connector that hands the channel one end of an in-memory stream, once.
struct InMemory(Option<DuplexStream>);

impl Service<http::Uri> for InMemory {
    type Response = TokioIo<DuplexStream>;
    type Error = io::Error;
    type Future = Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _: http::Uri) -> Self::Future {
        let stream = self.0.take().map(TokioIo::new);
        ready(stream.ok_or(io::Error::from(io::ErrorKind::NotConnected)))
    }
}

/// A channel to `service`, which a task of the current tokio runtime serves over an
/// in-memory stream. No socket is opened and no name is resolved: the endpoint's URI is a
/// placeholder the connector ignores. The channel connects on its first call, and the
/// task ends when the channel and its clones are dropped.
///
/// `service` is anything the tonic server serves: the generated `JobServiceServer`, or a
/// raw tower service of HTTP requests, such as a fake that reads the bytes of each call.
///
/// # Panics
/// When called outside a tokio runtime.
pub fn channel_to<S, B>(service: S) -> Channel
where
    S: Service<http::Request<Body>, Response = http::Response<B>> + Clone + Send + 'static,
    S::Future: Send,
    S::Error: Into<StdError> + Send + 'static,
    B: tonic::codegen::Body<Data = Bytes> + Send + 'static,
    B::Error: Into<StdError>,
{
    let (client_end, server_end) = tokio::io::duplex(BUFFER);
    let incoming = tokio_stream::once(Ok::<_, io::Error>(server_end));
    tokio::spawn(Server::builder().serve_with_incoming(service, incoming));
    Endpoint::from_static("http://in-process.invalid")
        .connect_with_connector_lazy(InMemory(Some(client_end)))
}
