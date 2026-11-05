//! Where the next chunk of a stream ends: a pure function of how many bytes wait and how
//! long the oldest of them has waited.

use std::time::Duration;

/// What decides a cut: the most bytes in one chunk, and how long a waiting byte may wait
/// for its chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CutLimits {
    /// The most bytes in one chunk (`MAX_CHUNK`, 1 MiB).
    pub chunk: usize,
    /// How long the oldest waiting byte may wait before a chunk is cut below `chunk`
    /// (`LOG_FLUSH`, 2 s).
    pub flush: Duration,
}

/// The length of the next chunk of a stream with `pending` bytes waiting, the oldest of which
/// was first seen `oldest_age` ago: `min(pending, chunk)` once `chunk` bytes wait or the oldest
/// has waited `flush` or longer, and `None` otherwise. Never an empty chunk, never one longer
/// than `limits.chunk`; a longer `pending` is cut in several, each starting where the last
/// ended. The end of a job cuts what is left by passing [`Duration::MAX`] as the age.
#[must_use]
pub fn cut(pending: u64, oldest_age: Duration, limits: CutLimits) -> Option<usize> {
    let chunk = u64::try_from(limits.chunk).unwrap_or(u64::MAX);
    let due = pending >= chunk || oldest_age >= limits.flush;
    let length = pending.min(chunk);
    (pending > 0 && due).then(|| usize::try_from(length).unwrap_or(limits.chunk))
}
