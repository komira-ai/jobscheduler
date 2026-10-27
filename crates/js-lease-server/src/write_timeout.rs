//! A byte stream whose writes may not stall.

use std::future::Future;
use std::io::{self, IoSlice};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::time::{Sleep, sleep};

/// A stream that fails a write which makes no progress for `limit`.
///
/// Reads pass through untouched (the request head has its own timeout). The clock of the
/// write side starts when a write, flush or shutdown cannot finish at once and stops the
/// moment one does, so a client that reads slowly but steadily is never cut and a client
/// that does not read at all is, with a [`io::ErrorKind::TimedOut`] error that ends the
/// connection and gives back its slot.
pub(crate) struct WriteTimeout<S> {
    inner: S,
    limit: Duration,
    stalled: Option<Pin<Box<Sleep>>>,
}

impl<S> WriteTimeout<S> {
    /// `inner`, with writes limited to `limit` without progress.
    pub(crate) fn new(inner: S, limit: Duration) -> Self {
        Self {
            inner,
            limit,
            stalled: None,
        }
    }

    /// Applies the clock to one poll of the write side: a finished poll stops it, an
    /// unfinished one starts it if it is not running, and an unfinished one that finds
    /// it run out is the timeout.
    fn watch<T>(
        &mut self,
        cx: &mut Context<'_>,
        polled: Poll<io::Result<T>>,
    ) -> Poll<io::Result<T>> {
        match polled {
            Poll::Ready(done) => {
                self.stalled = None;
                Poll::Ready(done)
            }
            Poll::Pending => {
                let limit = self.limit;
                let clock = self.stalled.get_or_insert_with(|| Box::pin(sleep(limit)));
                match clock.as_mut().poll(cx) {
                    Poll::Ready(()) => {
                        self.stalled = None;
                        Poll::Ready(Err(io::ErrorKind::TimedOut.into()))
                    }
                    Poll::Pending => Poll::Pending,
                }
            }
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for WriteTimeout<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for WriteTimeout<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let polled = Pin::new(&mut self.inner).poll_write(cx, buf);
        self.watch(cx, polled)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let polled = Pin::new(&mut self.inner).poll_write_vectored(cx, bufs);
        self.watch(cx, polled)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let polled = Pin::new(&mut self.inner).poll_flush(cx);
        self.watch(cx, polled)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let polled = Pin::new(&mut self.inner).poll_shutdown(cx);
        self.watch(cx, polled)
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};

    use super::*;

    /// A stream with a buffer of four bytes: a write fits until nobody reads.
    fn small(
        limit: Duration,
    ) -> (
        WriteTimeout<tokio::io::DuplexStream>,
        tokio::io::DuplexStream,
    ) {
        let (near, far) = duplex(4);
        (WriteTimeout::new(near, limit), far)
    }

    /// Asserts that a write that makes no progress fails with `TimedOut` once the limit
    /// has run out, that the stream works again as soon as the other side reads (the
    /// clock was stopped by the write that went through), and that a flush and a
    /// vectored write that go through are not cut.
    #[tokio::test]
    async fn a_write_without_progress_times_out_and_progress_stops_the_clock() {
        let (mut out, mut far) = small(Duration::from_millis(20));
        out.write_all(b"abcd").await.unwrap();
        let stalled = out.write_all(b"efgh").await.unwrap_err();
        assert_eq!(stalled.kind(), io::ErrorKind::TimedOut);

        let mut got = [0u8; 4];
        far.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"abcd");
        let parts = [IoSlice::new(b"ij"), IoSlice::new(b"kl")];
        assert_eq!(out.write_vectored(&parts).await.unwrap(), 4);
        out.flush().await.unwrap();
        assert!(out.is_write_vectored());
        far.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"ijkl");
    }

    /// Asserts that reads pass through to the stream and that a shutdown that goes
    /// through is not cut, closing the write side for the far end.
    #[tokio::test]
    async fn reads_pass_through_and_shutdown_goes_through() {
        let (mut out, mut far) = small(Duration::from_millis(20));
        far.write_all(b"hi").await.unwrap();
        let mut hello = [0u8; 2];
        out.read_exact(&mut hello).await.unwrap();
        assert_eq!(&hello, b"hi");

        out.write_all(b"ab").await.unwrap();
        out.shutdown().await.unwrap();
        let mut rest = Vec::new();
        far.read_to_end(&mut rest).await.unwrap();
        assert_eq!(rest, b"ab");
    }
}
