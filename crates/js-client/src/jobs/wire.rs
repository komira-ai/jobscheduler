//! The byte stream of a job client: dialled to the one origin, wrapped in TLS for an
//! `https://` origin, and watched for the first byte of each request it carries.
//!
//! The channel speaks HTTP/2 over the stream. A request starts with a `HEADERS` frame, and
//! nothing of it reaches the server before that frame does, so the stream counts the
//! `HEADERS` frames written to it (in the plain text, above TLS). A call compares the count
//! after a failure with the count before it: no new frame means the request was never
//! written.

use std::future::Future;
use std::io;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};

use hyper_util::rt::TokioIo;
use rustls::crypto::ring::default_provider;
use rustls::pki_types::ServerName;
use rustls::{ClientConfig, RootCertStore};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tonic::codegen::Service;
use tonic::codegen::http::Uri;

use super::JobError;
use crate::server_url::ServerUrl;

/// A byte stream a job client can speak over: what a [`Dialer`] opens.
pub trait Io: AsyncRead + AsyncWrite + Send + Unpin + 'static {}

impl<T: AsyncRead + AsyncWrite + Send + Unpin + 'static> Io for T {}

/// What dialling gives: the open stream, or why it could not be opened.
pub type Dialed = Pin<Box<dyn Future<Output = io::Result<Box<dyn Io>>> + Send>>;

/// How a job client opens its byte stream to the server's host and port: TCP for the
/// program ([`Dialer::tcp`]); a test hands in an in-memory stream instead. No proxy setting
/// is read: the stream goes to the host named and nowhere else.
#[derive(Clone)]
pub struct Dialer(Arc<Dial>);

/// What a [`Dialer`] calls: the host (without brackets) and the port, to the stream.
type Dial = dyn Fn(&str, u16) -> Dialed + Send + Sync;

impl Dialer {
    /// A TCP connection to the host and port.
    #[must_use]
    pub fn tcp() -> Self {
        Self::new(|host, port| {
            let host = host.to_owned();
            Box::pin(async move {
                let stream = TcpStream::connect((host.as_str(), port)).await?;
                Ok(Box::new(stream) as Box<dyn Io>)
            })
        })
    }

    /// A dialer that opens its stream with `dial`, given the host (without brackets) and the
    /// port of the server.
    pub fn new(dial: impl Fn(&str, u16) -> Dialed + Send + Sync + 'static) -> Self {
        Self(Arc::new(dial))
    }
}

/// What the channel connects with: the dialer, the origin's host and port, the TLS
/// settings for an `https://` origin, and the count of requests begun. The URI the channel
/// asks for is not read: every connection goes to the one origin.
#[derive(Clone)]
pub(crate) struct Connector {
    dialer: Dialer,
    host: String,
    port: u16,
    tls: Option<(TlsConnector, ServerName<'static>)>,
    begun: Arc<AtomicU64>,
}

impl Connector {
    /// The connector of `server` (whose base is `uri`), trusting the certificates of
    /// `ca_cert` (only those) or, without it, the public roots bundled in `webpki-roots`,
    /// never the platform's store.
    pub(crate) fn new(
        server: &ServerUrl,
        uri: &Uri,
        ca_cert: Option<&Path>,
        dialer: Dialer,
        begun: Arc<AtomicU64>,
    ) -> Result<Self, JobError> {
        let host = uri
            .host()
            .unwrap_or_default()
            .trim_matches(['[', ']'])
            .to_owned();
        let default_port = match server.https {
            true => 443,
            false => 80,
        };
        let port = uri.port_u16().unwrap_or(default_port);
        let tls = match server.https {
            true => Some(tls(&host, ca_cert)?),
            false => None,
        };
        Ok(Self {
            dialer,
            host,
            port,
            tls,
            begun,
        })
    }

    /// Dials, does the TLS handshake for an `https://` origin, and watches the stream.
    async fn open(self) -> io::Result<TokioIo<Watched>> {
        let raw = (self.dialer.0)(&self.host, self.port).await?;
        let stream: Box<dyn Io> = match self.tls {
            Some((connector, name)) => {
                let handshake = connector.connect(name, raw).await;
                let failed = |why| io::Error::other(format!("TLS with {}: {why}", self.host));
                Box::new(handshake.map_err(failed)?)
            }
            None => raw,
        };
        Ok(TokioIo::new(Watched::new(stream, self.begun)))
    }
}

impl Service<Uri> for Connector {
    type Response = TokioIo<Watched>;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = io::Result<TokioIo<Watched>>> + Send>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _: Uri) -> Self::Future {
        Box::pin(self.clone().open())
    }
}

/// The TLS settings for `host`: the roots, HTTP/2 by ALPN, no client certificate.
fn tls(
    host: &str,
    ca_cert: Option<&Path>,
) -> Result<(TlsConnector, ServerName<'static>), JobError> {
    let roots = roots(ca_cert)?;
    // The provider is ring's, which has every safe protocol version, so this does not fail.
    let builder = ClientConfig::builder_with_provider(Arc::new(default_provider()))
        .with_safe_default_protocol_versions()
        .ok()
        .ok_or(JobError::flag("--server", "no TLS version to speak"))?;
    let mut config = builder.with_root_certificates(roots).with_no_client_auth();
    config.alpn_protocols = vec![b"h2".to_vec()];
    let name = ServerName::try_from(host.to_owned())
        .ok()
        .ok_or(JobError::flag("--server", "is not a TLS server name"))?;
    Ok((TlsConnector::from(Arc::new(config)), name))
}

/// The roots to trust: the certificates of the `--ca-cert` PEM file when it is given, and
/// only those; otherwise the public roots bundled in `webpki-roots` (a Mozilla set, pinned
/// by `Cargo.lock`). The platform's own store is never read, as for the lease client.
fn roots(ca_cert: Option<&Path>) -> Result<RootCertStore, JobError> {
    let mut store = RootCertStore::empty();
    match ca_cert {
        Some(path) => {
            // Every certificate of the file must be one: one that does not parse is refused
            // rather than left out.
            let refuse = |why: rustls::Error| {
                JobError::flag("--ca-cert", format!("{}: {why}", path.display()))
            };
            crate::client::pem_roots(path)?
                .into_iter()
                .try_for_each(|cert| store.add(cert).map_err(refuse))?;
        }
        None => store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
    }
    Ok(store)
}

/// A stream that counts the requests begun on it: the `HEADERS` frames written.
pub(crate) struct Watched {
    inner: Box<dyn Io>,
    frames: Frames,
    begun: Arc<AtomicU64>,
}

impl Watched {
    fn new(inner: Box<dyn Io>, begun: Arc<AtomicU64>) -> Self {
        Self {
            inner,
            frames: Frames::default(),
            begun,
        }
    }
}

impl AsyncRead for Watched {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for Watched {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = &mut *self;
        let polled = Pin::new(&mut this.inner).poll_write(cx, buf);
        let accepted = match &polled {
            Poll::Ready(Ok(count)) => *count,
            Poll::Ready(Err(_)) | Poll::Pending => 0,
        };
        let begun = this.frames.feed(buf.get(..accepted).unwrap_or_default());
        this.begun.fetch_add(begun, Ordering::SeqCst);
        polled
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// The length of the HTTP/2 client preface (RFC 9113, section 3.4).
const PREFACE: usize = 24;

/// The type of a `HEADERS` frame.
const HEADERS: u8 = 0x1;

/// Where the bytes a client writes stand in the HTTP/2 framing: the preface, then frames,
/// each a header and its payload.
#[derive(Debug, Default)]
struct Frames {
    /// The bytes of the preface seen.
    preface: usize,
    /// The bytes of the current frame header seen.
    header: Vec<u8>,
    /// The bytes of the current frame's payload still to come.
    payload: usize,
}

impl Frames {
    /// Reads `bytes`, the next the client wrote: the number of `HEADERS` frames whose header
    /// they complete.
    fn feed(&mut self, bytes: &[u8]) -> u64 {
        bytes.iter().map(|byte| self.byte(*byte)).sum()
    }

    fn byte(&mut self, byte: u8) -> u64 {
        match (self.preface < PREFACE, self.payload > 0) {
            (true, _) => {
                self.preface += 1;
                0
            }
            (false, true) => {
                self.payload -= 1;
                0
            }
            (false, false) => self.header_byte(byte),
        }
    }

    fn header_byte(&mut self, byte: u8) -> u64 {
        self.header.push(byte);
        match *self.header.as_slice() {
            [high, middle, low, kind, _, _, _, _, _] => {
                self.payload =
                    (usize::from(high) << 16) | (usize::from(middle) << 8) | usize::from(low);
                self.header.clear();
                u64::from(kind == HEADERS)
            }
            _ => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A frame header: `length` bytes of payload, of `kind`, on stream 1.
    fn frame(kind: u8, length: u8) -> Vec<u8> {
        vec![0, 0, length, kind, 0, 0, 0, 0, 1]
    }

    /// Asserts the count of requests begun over a client's bytes cut at every place: the
    /// preface, a `SETTINGS` frame, a `HEADERS` frame and its payload, a `DATA` frame, a
    /// second `HEADERS` with no payload. It catches a preface counted as a frame, a payload
    /// read as a header, and a header split across two writes that is missed.
    #[test]
    fn headers_frames_are_counted_wherever_the_writes_cut() {
        let mut bytes = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n".to_vec();
        bytes.extend(frame(0x4, 6));
        bytes.extend([0; 6]);
        bytes.extend(frame(HEADERS, 3));
        // A payload that looks like a HEADERS frame header must not count.
        bytes.extend([0, 0, 0]);
        bytes.extend(frame(0x0, 9));
        bytes.extend(frame(HEADERS, 0));
        bytes.extend([0; 9]);
        bytes.extend(frame(HEADERS, 0));
        for cut in 0..=bytes.len() {
            let mut frames = Frames::default();
            let (first, second) = bytes.split_at(cut);
            assert_eq!(frames.feed(first) + frames.feed(second), 2, "cut at {cut}");
        }
    }

    /// Asserts the count at each point of a `HEADERS` frame written in pieces: nothing while
    /// its header is incomplete, one once its last header byte is written, and nothing more
    /// for its payload.
    #[test]
    fn a_headers_frame_counts_when_its_header_is_whole() {
        let mut frames = Frames::default();
        assert_eq!(frames.feed(&[0; PREFACE]), 0);
        let header = frame(HEADERS, 2);
        assert_eq!(frames.feed(&header[..5]), 0);
        assert_eq!(frames.feed(&header[5..8]), 0);
        assert_eq!(frames.feed(&header[8..]), 1);
        assert_eq!(frames.feed(&[0, 0]), 0);
    }

    /// Asserts that a write the stream refuses counts nothing, so a request whose bytes
    /// never left is not taken as written: the other end has gone, so the write fails.
    #[test]
    fn a_refused_write_counts_nothing() {
        let (client, server) = tokio::io::duplex(64);
        drop(server);
        let begun = Arc::new(AtomicU64::new(0));
        let mut watched = Watched::new(Box::new(client), Arc::clone(&begun));
        let mut bytes = vec![0; PREFACE];
        bytes.extend(frame(HEADERS, 0));
        let waker = std::task::Waker::noop();
        let polled = Pin::new(&mut watched).poll_write(&mut Context::from_waker(waker), &bytes);
        assert!(
            format!("{polled:?}").starts_with("Ready(Err("),
            "{polled:?}"
        );
        assert_eq!(begun.load(Ordering::SeqCst), 0);
    }
}
