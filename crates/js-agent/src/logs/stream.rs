//! One stream of a job as the uploader tails it: the capture file through a handle held for
//! the uploader's whole life, what of it was seen and when, and what was put.

use std::collections::VecDeque;
use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt as _;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use js_logstore::Stream;

use super::chunker::{CutLimits, cut};
use super::record::{Cut, Progress};

/// What the uploader does next for a stream: one store request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Action {
    /// Put the chunk of `len` bytes at `offset`, whose first byte was first seen at
    /// `first_at`.
    Chunk {
        offset: u64,
        len: usize,
        first_at: SystemTime,
    },
    /// Put the gap marker at `offset` for `length` bytes the capture could not keep.
    Gap { offset: u64, length: u64 },
    /// Put the final file: the whole stream, `len` bytes.
    Final { len: u64 },
    /// Delete the chunk at `offset`.
    Delete { offset: u64 },
}

/// The end of a stream, as the capture reported it once its drain was over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct End {
    /// The capture file's final length `L`.
    pub(super) length: u64,
    /// The bytes the capture could not keep: the stream's total less `L`.
    pub(super) dropped: u64,
}

/// One stream of a job, tailed by byte offset.
pub(super) struct Tail {
    pub(super) stream: Stream,
    file: Arc<File>,
    /// The file's length at the last poll.
    seen: u64,
    /// The offset of the first byte not yet put: where the next chunk begins.
    next: u64,
    /// When each run of bytes was first seen: `(offset of its first byte, the poll's time)`,
    /// oldest first; the first covers `next`.
    arrivals: VecDeque<(u64, SystemTime)>,
    /// The chunk cut and not yet put, `(offset, length)`: a retry puts the same one.
    cut: Option<(u64, usize)>,
    /// The offsets of the chunks put and not yet deleted, in order.
    put: VecDeque<u64>,
    /// The failed deletes of the first chunk of `put`.
    delete_failures: u32,
    end: Option<End>,
    gap_put: bool,
    final_put: bool,
}

impl Tail {
    /// The stream `stream`, read through `file`, from where `progress` says its upload is (the
    /// start of the stream for a new job, a progress record's for an adopted one). What the file
    /// holds past `next` is seen at the first poll, which is the first-byte time of those bytes;
    /// a cut in flight is put again as it was cut.
    pub(super) fn new(stream: Stream, file: Arc<File>, progress: Progress) -> Self {
        Self {
            stream,
            file,
            seen: progress.next,
            next: progress.next,
            arrivals: VecDeque::new(),
            cut: progress.cut.map(|cut| (cut.offset, cut.len)),
            put: progress.chunks.into(),
            delete_failures: 0,
            end: None,
            gap_put: progress.gap_done,
            final_put: progress.final_done,
        }
    }

    /// Where the stream's upload is, as its progress record says it.
    pub(super) fn progress(&self) -> Progress {
        Progress {
            next: self.next,
            cut: self.cut.map(|(offset, len)| Cut { offset, len }),
            chunks: self.put.iter().copied().collect(),
            gap_done: self.gap_put,
            final_done: self.final_put,
        }
    }

    /// Whether chunks put are still to be deleted.
    pub(super) fn deleting(&self) -> bool {
        !self.put.is_empty()
    }

    /// Reads the file's length and notes growth as an arrival at `now`.
    pub(super) fn poll(&mut self, now: SystemTime) {
        let length = self.file.metadata().map_or(self.seen, |meta| meta.len());
        if length > self.seen {
            self.arrivals.push_back((self.seen, now));
            self.seen = length;
        }
    }

    /// Records the end of the stream. A length below what was put already is taken as what
    /// was put.
    pub(super) fn end(&mut self, end: End) {
        self.end = Some(End {
            length: end.length.max(self.next),
            dropped: end.dropped,
        });
    }

    /// When the byte at `offset` was first seen: the time of the last arrival at or below it.
    fn first_seen(&self, offset: u64) -> Option<SystemTime> {
        self.arrivals
            .iter()
            .rev()
            .find(|(at, _)| *at <= offset)
            .map(|(_, when)| *when)
    }

    /// The next request for the stream at `now`, or `None` when nothing is due. While the job
    /// runs that is a chunk once [`cut`] says so; once it has ended, every byte up to the
    /// file's length is cut whatever its age, then the gap marker of the bytes dropped, then,
    /// for a stream that is whole, not empty and at most `max_put` bytes, the final, and after
    /// it the deletes of the chunks put. A cut stays until its put succeeded.
    pub(super) fn action(
        &mut self,
        now: SystemTime,
        limits: CutLimits,
        max_put: u64,
    ) -> Option<Action> {
        if self.cut.is_none() {
            let (until, age) = match self.end {
                Some(end) => (end.length, Duration::MAX),
                None => {
                    let first = self.first_seen(self.next).unwrap_or(now);
                    (
                        self.seen,
                        now.duration_since(first).unwrap_or(Duration::ZERO),
                    )
                }
            };
            let pending = until.saturating_sub(self.next);
            self.cut = cut(pending, age, limits).map(|len| (self.next, len));
        }
        if let Some((offset, len)) = self.cut {
            let first_at = self.first_seen(offset).unwrap_or(now);
            return Some(Action::Chunk {
                offset,
                len,
                first_at,
            });
        }
        let end = self.end?;
        if end.dropped > 0 && !self.gap_put {
            return Some(Action::Gap {
                offset: end.length,
                length: end.dropped,
            });
        }
        let whole = end.dropped == 0 && end.length > 0 && end.length <= max_put;
        if whole && !self.final_put {
            return Some(Action::Final { len: end.length });
        }
        self.put
            .front()
            .filter(|_| self.final_put)
            .map(|offset| Action::Delete { offset: *offset })
    }

    /// Whether nothing is left to do for the stream, asked only once it has ended.
    pub(super) fn done(&mut self, now: SystemTime, limits: CutLimits, max_put: u64) -> bool {
        self.action(now, limits, max_put).is_none()
    }

    /// Records that `action` succeeded.
    pub(super) fn succeeded(&mut self, action: Action) {
        match action {
            Action::Chunk { offset, len, .. } => {
                self.cut = None;
                self.put.push_back(offset);
                self.next = offset.saturating_add(u64::try_from(len).unwrap_or(u64::MAX));
                while self.arrivals.get(1).is_some_and(|(at, _)| *at <= self.next) {
                    self.arrivals.pop_front();
                }
            }
            Action::Gap { .. } => self.gap_put = true,
            Action::Final { .. } => self.final_put = true,
            Action::Delete { .. } => {
                self.put.pop_front();
                self.delete_failures = 0;
            }
        }
    }

    /// Records that a delete failed: after `tries` failures of one chunk it is left (the
    /// final is authoritative) and the next is tried.
    pub(super) fn delete_failed(&mut self, tries: u32) {
        self.delete_failures += 1;
        if self.delete_failures >= tries {
            self.put.pop_front();
            self.delete_failures = 0;
        }
    }

    /// The `len` bytes of the file from `offset`, read through the held handle (a file that
    /// was unlinked since is still read).
    pub(super) fn read(&self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let mut body = vec![0; len];
        self.file.read_exact_at(&mut body, offset)?;
        Ok(body)
    }
}
