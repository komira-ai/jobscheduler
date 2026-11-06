//! The names of a job's log objects, and the body of a gap marker.
//!
//! Let `prefix` be the base a job's objects are under, the host's prefix (the configured log
//! prefix and the host's segment, `js_logmint::host_prefix`, without its trailing `/`), and `P`
//! the job's prefix, `prefix/job_id/` (just `job_id/` when the base is empty). A job's objects
//! are:
//!
//! | object | key | body |
//! |---|---|---|
//! | chunk | `P` + `stdout/` or `stderr/` + `offset20` | the raw bytes, 1 to [`MAX_CHUNK_BYTES`] |
//! | gap marker | the chunk's key + `.gap` | `{"length":N}` ([`encode_gap`]) |
//! | final | `P` + `stdout.log` or `stderr.log` | the whole stream, at most [`MAX_PUT_BYTES`] |
//!
//! `offset20` is the decimal offset of the object's first byte in its stream, zero padded to
//! [`OFFSET_WIDTH`] digits, so that every `u64` fits and the names of one stream sort in offset
//! order as byte strings. A chunk carries its first byte's arrival time as the object metadata
//! [`FIRST_BYTE_AT_MS`].
//!
//! The prefix and the job id are checked so that a key built here never names another job's
//! objects: the prefix is segments of `[A-Za-z0-9._-]`, none empty, `.` or `..`, at most
//! [`MAX_PREFIX_LEN`] bytes; the job id is 1 to [`MAX_JOB_ID_LEN`] bytes of the same alphabet
//! beginning with a letter or a digit, so it holds no `/` and is never `.` or `..`. Neither may
//! hold a character of a resource pattern (`*`, `?`, `$`, `{`): a session policy names the prefix
//! in one.
//!
//! Everything here is pure: no clock, no I/O.

use serde_json::Value;

/// The digits of an offset in an object name: every `u64` fits.
pub const OFFSET_WIDTH: usize = 20;

/// The most bytes in one chunk.
pub const MAX_CHUNK_BYTES: usize = 1_048_576;

/// The most bytes the client puts in one request (a final file is one put).
pub const MAX_PUT_BYTES: usize = 64 * 1024 * 1024;

/// The longest log prefix, in bytes.
pub const MAX_PREFIX_LEN: usize = 512;

/// The longest job id a job prefix is built for, in bytes.
pub const MAX_JOB_ID_LEN: usize = 63;

/// The object metadata of a chunk: decimal milliseconds since the Unix epoch of the poll that
/// first saw the chunk's first byte. On the wire it is the header `x-amz-meta-first-byte-at-ms`.
pub const FIRST_BYTE_AT_MS: &str = "first-byte-at-ms";

/// What follows a chunk's name to make a gap marker's.
pub const GAP_SUFFIX: &str = ".gap";

/// Why a prefix, a job id or a gap marker was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LayoutError {
    /// The log prefix is longer than [`MAX_PREFIX_LEN`] bytes.
    #[error("the log prefix is longer than {MAX_PREFIX_LEN} bytes")]
    PrefixTooLong,
    /// The log prefix has a leading or trailing `/`, an empty segment, a `.` or `..` segment,
    /// or a byte outside `[A-Za-z0-9._-]`.
    #[error(
        "the log prefix must be `/`-separated segments of [A-Za-z0-9._-], none empty, `.` or `..`"
    )]
    Prefix,
    /// The job id is not 1 to [`MAX_JOB_ID_LEN`] bytes of `[A-Za-z0-9._-]` beginning with a
    /// letter or a digit.
    #[error(
        "the job id must be 1 to {MAX_JOB_ID_LEN} bytes of [A-Za-z0-9._-] beginning with a letter or digit"
    )]
    JobId,
    /// A job prefix's text does not end in `/`, or what it holds is not a valid prefix and job
    /// id.
    #[error("a job prefix is `prefix/job_id/` or `job_id/`")]
    JobPrefix,
    /// A gap is empty, or its end is past `u64::MAX`.
    #[error("a gap is at least one byte long and ends at most at u64::MAX")]
    Gap,
    /// A gap marker's body is not `{"length":N}`.
    #[error("a gap marker's body must be {{\"length\":N}}")]
    GapBody,
}

/// One of a job's two output streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Stream {
    /// The job's standard output.
    Stdout,
    /// The job's standard error.
    Stderr,
}

impl Stream {
    /// The stream's name in a key: `stdout` or `stderr`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }
}

/// A job's prefix `P`: `prefix/job_id/` under the base `prefix` (the host's prefix), or
/// `job_id/` when the base is empty. Every key of the job's objects begins with it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JobPrefix(String);

impl JobPrefix {
    /// The prefix of the job `job_id` under the log prefix `base`.
    ///
    /// # Errors
    ///
    /// [`LayoutError::PrefixTooLong`], [`LayoutError::Prefix`] or [`LayoutError::JobId`] when
    /// `base` or `job_id` breaks the rules of the module docs.
    pub fn new(base: &str, job_id: &str) -> Result<Self, LayoutError> {
        if base.len() > MAX_PREFIX_LEN {
            return Err(LayoutError::PrefixTooLong);
        }
        if !base.is_empty() && !base.split('/').all(segment_ok) {
            return Err(LayoutError::Prefix);
        }
        if !job_id_ok(job_id) {
            return Err(LayoutError::JobId);
        }
        let text = match base.is_empty() {
            true => format!("{job_id}/"),
            false => format!("{base}/{job_id}/"),
        };
        Ok(Self(text))
    }

    /// The job prefix whose text is `text` (`prefix/job_id/` or `job_id/`), as a job's start
    /// carries it.
    ///
    /// # Errors
    ///
    /// [`LayoutError::JobPrefix`] when `text` does not end in `/`; otherwise what
    /// [`JobPrefix::new`] says of the prefix and the job id it holds.
    pub fn parse(text: &str) -> Result<Self, LayoutError> {
        let inner = text.strip_suffix('/').ok_or(LayoutError::JobPrefix)?;
        let (base, job_id) = inner.rsplit_once('/').unwrap_or(("", inner));
        Self::new(base, job_id)
    }

    /// The text of the prefix, ending in `/`.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The prefix of every chunk and gap marker of `stream`: `P` + `stdout/` or `stderr/`.
    pub fn stream_prefix(&self, stream: Stream) -> String {
        format!("{}{}/", self.0, stream.name())
    }

    /// The key of the chunk of `stream` whose first byte is at `offset`.
    pub fn chunk_key(&self, stream: Stream, offset: u64) -> String {
        format!("{}{}", self.stream_prefix(stream), offset_name(offset))
    }

    /// The key of the gap marker of `stream` at `offset`.
    pub fn gap_key(&self, stream: Stream, offset: u64) -> String {
        format!("{}{GAP_SUFFIX}", self.chunk_key(stream, offset))
    }

    /// The key of the final file of `stream`: `P` + `stdout.log` or `stderr.log`.
    pub fn final_key(&self, stream: Stream) -> String {
        format!("{}{}.log", self.0, stream.name())
    }
}

/// What the last segment of a chunk's or a gap marker's key says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkName {
    /// The offset of the object's first byte in its stream.
    pub offset: u64,
    /// Whether the object is a gap marker.
    pub is_gap: bool,
}

/// What `name`, the part of a key after the stream's prefix, says: exactly [`OFFSET_WIDTH`]
/// decimal digits, then [`GAP_SUFFIX`] for a gap marker. A sign, a shorter or longer number, a
/// number above `u64::MAX` and any other suffix are `None`.
pub fn parse_chunk_name(name: &str) -> Option<ChunkName> {
    let (digits, is_gap) = name
        .strip_suffix(GAP_SUFFIX)
        .map_or((name, false), |digits| (digits, true));
    let well_formed = digits.len() == OFFSET_WIDTH && digits.bytes().all(|b| b.is_ascii_digit());
    well_formed
        .then(|| digits.parse().ok())
        .flatten()
        .map(|offset| ChunkName { offset, is_gap })
}

/// Bytes of a stream that were never uploaded: `[offset, offset + length)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gap {
    offset: u64,
    length: u64,
}

impl Gap {
    /// The gap of `length` bytes from `offset`.
    ///
    /// # Errors
    ///
    /// [`LayoutError::Gap`] when `length` is 0 or the gap would end past `u64::MAX`.
    pub fn new(offset: u64, length: u64) -> Result<Self, LayoutError> {
        match length > 0 && offset.checked_add(length).is_some() {
            true => Ok(Self { offset, length }),
            false => Err(LayoutError::Gap),
        }
    }

    /// The offset of the first byte that was never uploaded.
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// How many bytes were never uploaded.
    pub fn length(&self) -> u64 {
        self.length
    }
}

/// The body of `gap`'s marker: exactly `{"length":N}`.
pub fn encode_gap(gap: &Gap) -> Vec<u8> {
    format!("{{\"length\":{}}}", gap.length).into_bytes()
}

/// The gap a marker at `offset` with the body `body` describes. The body is a JSON object with
/// the one member `length`, an integer of at least 1.
///
/// # Errors
///
/// [`LayoutError::GapBody`] when the body is not such an object; [`LayoutError::Gap`] when the
/// length is 0 or the gap would end past `u64::MAX`.
pub fn decode_gap(offset: u64, body: &[u8]) -> Result<Gap, LayoutError> {
    let length = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| {
            let object = value.as_object().filter(|object| object.len() == 1)?;
            object.get("length").and_then(Value::as_u64)
        })
        .ok_or(LayoutError::GapBody)?;
    Gap::new(offset, length)
}

/// `offset` as [`OFFSET_WIDTH`] decimal digits.
fn offset_name(offset: u64) -> String {
    format!("{offset:0OFFSET_WIDTH$}")
}

/// Whether `byte` is in `[A-Za-z0-9._-]`.
fn allowed(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')
}

/// Whether `segment` is a segment a log prefix may hold.
pub(crate) fn segment_ok(segment: &str) -> bool {
    !segment.is_empty() && segment != "." && segment != ".." && segment.bytes().all(allowed)
}

/// Whether a job prefix may be built for `job_id`.
fn job_id_ok(job_id: &str) -> bool {
    job_id.len() <= MAX_JOB_ID_LEN
        && job_id
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphanumeric())
        && job_id.bytes().all(allowed)
}
