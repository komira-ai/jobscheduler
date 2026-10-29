//! The compressed side of a layer: the zstd codec with the limits of the bundle format
//! (section 1.1), and the error every stream failure is reported as.
//!
//! A layer is exactly one zstd frame whose window is at most 8 MiB (the window sets the
//! decoder's memory), with no dictionary, and with nothing after the frame (no second
//! frame, no skippable frame, no stray byte). The frame header is read and checked here
//! before any byte reaches the decoder, and the decoder is told the same window limit.
//! The ceiling on the decompressed bytes is not here: the unpacker counts the bytes it
//! is given, whatever their encoding.

use std::io::{self, Cursor, Read};

use ruzstd::decoding::{FrameDecoder, StreamingDecoder};
use ruzstd::encoding::{CompressionLevel, compress_to_vec};

/// The largest zstd window a layer may use, in bytes.
pub const MAX_WINDOW_BYTES: u64 = 8 << 20;

/// The magic number of a zstd frame.
const FRAME_MAGIC: u32 = 0xFD2F_B528;
/// A skippable frame's magic number is this with any value in the low four bits.
const SKIPPABLE_MAGIC: u32 = 0x184D_2A50;
/// The bytes of the frame header that are always there: magic and descriptor.
const FIXED_HEAD: usize = 5;
/// The bytes of the content size field, by its two-bit flag, for a frame that is not
/// single-segment (first row) and one that is (second row, where flag 0 means one byte).
const CONTENT_SIZE_LEN: [[usize; 4]; 2] = [[0, 2, 4, 8], [1, 2, 4, 8]];
/// What a two-byte content size field adds to its value.
const CONTENT_SIZE_BIAS: [u64; 4] = [0, 256, 0, 0];

/// Why a stream of a layer cannot be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StreamError {
    /// The stream ended before it was complete.
    #[error("the stream ended early")]
    Truncated,
    /// The decompressed stream passed its ceiling.
    #[error("the decompressed stream passed its ceiling of {max} bytes")]
    Ceiling {
        /// The ceiling, in bytes.
        max: u64,
    },
    /// The stream starts with a skippable frame.
    #[error("the stream has a skippable zstd frame")]
    SkippableFrame,
    /// The stream does not start with a zstd frame.
    #[error("the stream is not a zstd frame")]
    NotZstd,
    /// The frame names a dictionary.
    #[error("the zstd frame names a dictionary")]
    Dictionary,
    /// The frame's window is over the limit.
    #[error("the zstd window of {size} bytes is over the limit of {max}")]
    Window {
        /// The window the frame asks for, in bytes.
        size: u64,
        /// The limit, in bytes.
        max: u64,
    },
    /// Bytes follow the one frame.
    #[error("bytes follow the zstd frame")]
    TrailingBytes,
    /// The source could not be read.
    #[error("reading the source: {0}")]
    Read(String),
    /// The frame is malformed.
    #[error("malformed zstd: {0}")]
    Corrupt(String),
}

/// Compresses `tar` into the one frame a layer is: fixed level, no dictionary, a content
/// checksum, and no field that depends on the host or the time, so equal input gives equal
/// bytes.
#[must_use]
pub fn compress(tar: &[u8]) -> Vec<u8> {
    todo!("{}", tar.len())
}

/// Reads `buf.len()` bytes, or says why not.
fn read_full(source: &mut dyn Read, buf: &mut [u8]) -> Result<(), StreamError> {
    source.read_exact(buf).map_err(|e| match e.kind() {
        io::ErrorKind::UnexpectedEof => StreamError::Truncated,
        _ => StreamError::Read(e.to_string()),
    })
}

/// A decoder failure as the error of the stream.
fn corrupt(error: impl std::fmt::Display) -> StreamError {
    StreamError::Corrupt(error.to_string())
}

/// Reads and checks the frame header from `source`, returning its bytes.
fn read_head(source: &mut dyn Read) -> Result<Vec<u8>, StreamError> {
    let mut fixed = [0u8; FIXED_HEAD];
    read_full(source, &mut fixed)?;
    let magic = u32::from_le_bytes([fixed[0], fixed[1], fixed[2], fixed[3]]);
    if magic & 0xFFFF_FFF0 == SKIPPABLE_MAGIC {
        return Err(StreamError::SkippableFrame);
    }
    if magic != FRAME_MAGIC {
        return Err(StreamError::NotZstd);
    }
    let descriptor = fixed[4];
    if descriptor & 0x03 != 0 {
        return Err(StreamError::Dictionary);
    }
    if descriptor & 0x08 != 0 {
        return Err(corrupt("a reserved bit is set"));
    }
    let single = descriptor & 0x20 != 0;
    let flag = usize::from(descriptor >> 6);
    let window_len = usize::from(!single);
    let mut rest = vec![0u8; window_len + CONTENT_SIZE_LEN[usize::from(single)][flag]];
    read_full(source, &mut rest)?;
    let (window_byte, size_bytes) = rest.split_at(window_len);
    let content_size = size_bytes
        .iter()
        .rev()
        .fold(0u64, |acc, byte| (acc << 8) | u64::from(*byte))
        + CONTENT_SIZE_BIAS[flag];
    let descriptor_byte = u64::from(window_byte.first().copied().unwrap_or(0));
    let base = 1u64 << (10 + (descriptor_byte >> 3));
    let windowed = base + (base >> 3) * (descriptor_byte & 7);
    let window = [windowed, content_size][usize::from(single)];
    if window > MAX_WINDOW_BYTES {
        return Err(StreamError::Window {
            size: window,
            max: MAX_WINDOW_BYTES,
        });
    }
    let mut head = fixed.to_vec();
    head.extend_from_slice(&rest);
    Ok(head)
}

/// What the zstd decoder reads: the header bytes already consumed, then the source.
struct Source<'a> {
    head: Cursor<Vec<u8>>,
    rest: &'a mut dyn Read,
    ended: bool,
}

impl Read for Source<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let from_head = self.head.read(buf)?;
        if from_head > 0 {
            return Ok(from_head);
        }
        let from_rest = self.rest.read(buf)?;
        self.ended |= from_rest == 0;
        Ok(from_rest)
    }
}

/// A reader of the decompressed bytes of one zstd frame, within the limits of a layer.
/// Every failure comes out of `read` as an [`io::Error`] whose source is a
/// [`StreamError`].
pub struct Decoder<'a> {
    frame: StreamingDecoder<Source<'a>, FrameDecoder>,
}

impl<'a> Decoder<'a> {
    /// Reads the frame header from `source` and checks it: a zstd frame (not a skippable
    /// one), no dictionary, a window of at most [`MAX_WINDOW_BYTES`].
    ///
    /// # Errors
    /// The header is missing, malformed or over a limit.
    pub fn new(source: &'a mut dyn Read) -> Result<Decoder<'a>, StreamError> {
        let _ = source;
        todo!()
    }
}

impl Read for Decoder<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let read = self.frame.read(buf);
        let count = read.map_err(|e| {
            let ended = self.frame.get_ref().ended;
            io::Error::other(if ended {
                StreamError::Truncated
            } else {
                corrupt(e)
            })
        })?;
        if count == 0 {
            // The frame is complete: nothing may follow it.
            let mut probe = [0u8; 1];
            let after = self.frame.get_mut().read(&mut probe)?;
            if after != 0 {
                return Err(io::Error::other(StreamError::TrailingBytes));
            }
        }
        Ok(count)
    }
}
