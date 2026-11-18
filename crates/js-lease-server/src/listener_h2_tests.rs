//! An HTTP/2 client that cancels requests the server has not taken up yet keeps its
//! connection up to [`MAX_STREAMS`] such requests, and is told to calm down one past it.
//!
//! The client is written by hand, frame by frame, over an in-memory pipe, and the whole of
//! what it sends is queued before the server reads a byte: the server then meets every
//! request and every cancel before it can take any request up, which is what a server whose
//! workers are held meets when it reads again. The order is the test's, not the scheduler's.

use axum::routing::get;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream, duplex};

use super::*;

/// Room in the pipe for everything the client sends and the server answers.
const PIPE: usize = 512 * 1024;

/// The frame types the tests write or look for (RFC 9113, section 6).
const HEADERS: u8 = 0x1;
const RST_STREAM: u8 = 0x3;
const SETTINGS: u8 = 0x4;
const GOAWAY: u8 = 0x7;

/// The flags of a HEADERS frame that holds the whole request: END_STREAM and END_HEADERS.
const WHOLE: u8 = 0x1 | 0x4;

/// The error codes the tests send or look for (RFC 9113, section 7).
const CANCEL: u32 = 0x8;
const ENHANCE_YOUR_CALM: u32 = 0xb;

/// What the client saw of its last request.
#[derive(Debug, PartialEq, Eq)]
enum Seen {
    /// The answer's head, on the last request's stream.
    Answered,
    /// The server ended the connection with this error code.
    GoAway(u32),
    /// The connection ended with neither.
    Closed,
}

/// One frame: its 9-byte head (length, type, flags, stream) and `payload`.
fn frame(kind: u8, flags: u8, stream: u32, payload: &[u8]) -> Vec<u8> {
    let length = u32::try_from(payload.len()).unwrap().to_be_bytes();
    let mut frame = vec![length[1], length[2], length[3], kind, flags];
    frame.extend_from_slice(&stream.to_be_bytes());
    frame.extend_from_slice(payload);
    frame
}

/// The HEADERS frame of `GET http://t/answer` on `stream`, with no body. The header block
/// is HPACK (RFC 7541) with no Huffman coding: `:method GET` and `:scheme http` from the
/// static table, `:path` and `:authority` as literals naming a static-table name.
fn get_answer(stream: u32) -> Vec<u8> {
    let mut block = vec![0x82, 0x86];
    block.extend_from_slice(&[0x04, 7]);
    block.extend_from_slice(b"/answer");
    block.extend_from_slice(&[0x01, 1, b't']);
    frame(HEADERS, WHOLE, stream, &block)
}

/// Everything the client sends: the preface and empty settings; `cancelled` requests, each
/// cancelled (RST_STREAM CANCEL) right after it is sent; then one more request, which it
/// waits on. Returns the bytes and the last request's stream.
fn client(cancelled: u32) -> (Vec<u8>, u32) {
    let mut sent = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n".to_vec();
    sent.extend(frame(SETTINGS, 0, 0, &[]));
    for n in 0..cancelled {
        let stream = 2 * n + 1;
        sent.extend(get_answer(stream));
        sent.extend(frame(RST_STREAM, 0, stream, &CANCEL.to_be_bytes()));
    }
    let last = 2 * cancelled + 1;
    sent.extend(get_answer(last));
    (sent, last)
}

/// Reads the server's frames until the head of the answer on `last`, a GOAWAY, or the end
/// of the connection.
async fn seen(pipe: &mut DuplexStream, last: u32) -> Seen {
    loop {
        let mut head = [0u8; 9];
        if pipe.read_exact(&mut head).await.is_err() {
            return Seen::Closed;
        }
        let length = u32::from_be_bytes([0, head[0], head[1], head[2]]);
        let mut payload = vec![0u8; usize::try_from(length).unwrap()];
        pipe.read_exact(&mut payload).await.unwrap();
        let stream = u32::from_be_bytes([head[5], head[6], head[7], head[8]]) & 0x7fff_ffff;
        match head[3] {
            GOAWAY => {
                let code = u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]);
                return Seen::GoAway(code);
            }
            HEADERS if stream == last => return Seen::Answered,
            _ => {}
        }
    }
}

/// Serves one HTTP/2 connection whose client sent [`client`]`(cancelled)` before the server
/// read a byte, and returns what the client saw of its last request.
async fn after_cancelling(cancelled: u32) -> Seen {
    let (mut pipe, server) = duplex(PIPE);
    let (sent, last) = client(cancelled);
    pipe.write_all(&sent).await.unwrap();
    let app = Router::new().route("/answer", get(|| async { "answer" }));
    let (_stop, stopping) = watch::channel(());
    let (_cut, cut) = watch::channel(());
    let gate = Arc::new(Semaphore::new(1));
    let permit = Arc::new(gate.try_acquire_owned().unwrap());
    let streams = Streams { permit, cut };
    let served = talk_both(Box::pin(server), app, Limits::default(), stopping, streams);
    let read = async move {
        let seen = seen(&mut pipe, last).await;
        // The end of the client ends the connection, and with it `served`.
        drop(pipe);
        seen
    };
    let ((), seen) = tokio::join!(served, read);
    seen
}

/// Asserts that a client that cancelled [`MAX_STREAMS`] requests the server had not taken
/// up keeps its connection: its next request on it is answered. Catches a listener held to
/// the protocol library's default of 20 such requests, which ends the connection with
/// `ENHANCE_YOUR_CALM` and fails every call on it (`RESOURCE_EXHAUSTED` to a gRPC client).
#[tokio::test]
async fn a_client_that_cancels_as_many_requests_as_it_may_open_keeps_its_connection() {
    let cancelled = u32::from(MAX_STREAMS);
    assert_eq!(after_cancelling(cancelled).await, Seen::Answered);
}

/// Asserts the low edge: a client that cancelled 21 requests (one past the library's default
/// bound) keeps its connection, and one that cancelled none is answered.
#[tokio::test]
async fn a_client_past_the_library_default_keeps_its_connection() {
    assert_eq!(after_cancelling(0).await, Seen::Answered);
    assert_eq!(after_cancelling(21).await, Seen::Answered);
}

/// Asserts that the bound still holds one past [`MAX_STREAMS`]: the connection is ended
/// with `ENHANCE_YOUR_CALM`, so a client that opens and cancels requests faster than the
/// server takes them up (the "rapid reset" flood) cannot make it hold more.
#[tokio::test]
async fn a_client_past_the_bound_is_told_to_calm_down() {
    let cancelled = u32::from(MAX_STREAMS) + 1;
    assert_eq!(
        after_cancelling(cancelled).await,
        Seen::GoAway(ENHANCE_YOUR_CALM)
    );
}
