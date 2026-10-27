//! A byte stream whose reads and writes may not stall.

use std::future::Future;
use std::io::{self, IoSlice};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::time::{Sleep, sleep};

/// The clock of one direction of a stream: it runs while the direction cannot make
/// progress and stops the moment it can.
struct Stall {
    limit: Duration,
    running: Option<Pin<Box<Sleep>>>,
}

impl Stall {
    fn new(limit: Duration) -> Self {
        Self {
            limit,
            running: None,
        }
    }

    /// Applies the clock to one poll: a finished poll stops it, an unfinished one
    /// starts it if it is not running, and an unfinished one that finds it run out is
    /// the timeout.
    fn watch<T>(
        &mut self,
        cx: &mut Context<'_>,
        polled: Poll<io::Result<T>>,
    ) -> Poll<io::Result<T>> {
        match polled {
            Poll::Ready(done) => {
                self.running = None;
                Poll::Ready(done)
            }
            Poll::Pending => {
                let limit = self.limit;
                let clock = self.running.get_or_insert_with(|| Box::pin(sleep(limit)));
                match clock.as_mut().poll(cx) {
                    Poll::Ready(()) => {
                        self.running = None;
                        Poll::Ready(Err(io::ErrorKind::TimedOut.into()))
                    }
                    Poll::Pending => Poll::Pending,
                }
            }
        }
    }
}

/// A stream that fails a read or a write which makes no progress for its limit.
///
/// The clock of a direction starts when a read (or a write, flush or shutdown) cannot
/// finish at once and stops the moment one does, so a client that sends or reads
/// slowly but steadily is never cut and a client that stops is, with an
/// [`io::ErrorKind::TimedOut`] error that ends the connection and gives back its slot.
/// A read waits for the client by nature, so a read limit also bounds the wait for the
/// next request on an idle connection: it belongs above the limit on the request head,
/// which is what closes an idle connection first.
pub(crate) struct IoTimeout<S> {
    inner: S,
    reading: Stall,
    writing: Stall,
}

impl<S> IoTimeout<S> {
    /// `inner`, with reads limited to `read` without progress and writes to `write`.
    pub(crate) fn new(inner: S, read: Duration, write: Duration) -> Self {
        Self {
            inner,
            reading: Stall::new(read),
            writing: Stall::new(write),
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for IoTimeout<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let polled = Pin::new(&mut self.inner).poll_read(cx, buf);
        self.reading.watch(cx, polled)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for IoTimeout<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let polled = Pin::new(&mut self.inner).poll_write(cx, buf);
        self.writing.watch(cx, polled)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let polled = Pin::new(&mut self.inner).poll_write_vectored(cx, bufs);
        self.writing.watch(cx, polled)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let polled = Pin::new(&mut self.inner).poll_flush(cx);
        self.writing.watch(cx, polled)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let polled = Pin::new(&mut self.inner).poll_shutdown(cx);
        self.writing.watch(cx, polled)
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};

    use super::*;

    /// A limit that runs out, and one that cannot within a test, for the direction a
    /// test does not exercise.
    const SHORT: Duration = Duration::from_millis(20);
    const LONG: Duration = Duration::from_secs(3600);

    /// A stream with a buffer of four bytes: a write fits until nobody reads.
    fn small(
        read: Duration,
        write: Duration,
    ) -> (IoTimeout<tokio::io::DuplexStream>, tokio::io::DuplexStream) {
        let (near, far) = duplex(4);
        (IoTimeout::new(near, read, write), far)
    }

    /// Asserts that a write that makes no progress fails with `TimedOut` once the limit
    /// has run out, that the stream works again as soon as the other side reads (the
    /// clock was stopped by the write that went through), and that a flush and a
    /// vectored write that go through are not cut.
    #[tokio::test]
    async fn a_write_without_progress_times_out_and_progress_stops_the_clock() {
        let (mut out, mut far) = small(LONG, SHORT);
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

    /// Asserts that a read that makes no progress fails with `TimedOut` once the limit
    /// has run out, that a read that goes through stops the clock (the same stream reads
    /// again, and the next stall times out afresh), and that the end of the stream is a
    /// read like any other, not a timeout.
    #[tokio::test]
    async fn a_read_without_progress_times_out_and_progress_stops_the_clock() {
        let (mut near, mut far) = small(SHORT, LONG);
        let mut got = [0u8; 2];
        let stalled = near.read_exact(&mut got).await.unwrap_err();
        assert_eq!(stalled.kind(), io::ErrorKind::TimedOut);

        far.write_all(b"hi").await.unwrap();
        near.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"hi");
        let again = near.read_exact(&mut got).await.unwrap_err();
        assert_eq!(again.kind(), io::ErrorKind::TimedOut);

        drop(far);
        let mut rest = Vec::new();
        near.read_to_end(&mut rest).await.unwrap();
        assert!(rest.is_empty());
    }

    /// Asserts that the two directions have their own clocks and limits: after a read
    /// has timed out, writes and a shutdown that go through are not cut (the far end
    /// sees what was written and then the end), and after a write has timed out, a read
    /// that goes through is not cut.
    #[tokio::test]
    async fn the_directions_are_timed_apart_and_shutdown_goes_through() {
        let (mut out, mut far) = small(SHORT, LONG);
        let mut got = [0u8; 2];
        let idle = out.read_exact(&mut got).await.unwrap_err();
        assert_eq!(idle.kind(), io::ErrorKind::TimedOut);
        out.write_all(b"ab").await.unwrap();
        out.shutdown().await.unwrap();
        let mut rest = Vec::new();
        far.read_to_end(&mut rest).await.unwrap();
        assert_eq!(rest, b"ab");

        let (mut out, mut far) = small(LONG, SHORT);
        out.write_all(b"abcd").await.unwrap();
        let full = out.write_all(b"efgh").await.unwrap_err();
        assert_eq!(full.kind(), io::ErrorKind::TimedOut);
        far.write_all(b"hi").await.unwrap();
        out.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"hi");
    }
}
