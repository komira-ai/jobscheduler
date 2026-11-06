//! One page of a job's stream, read from the log store with the server's read key
//! (`docs/design/live-logs.md`, section 3.6). Every call here is a blocking request of the
//! store client; the caller runs it on the pool for blocking calls, with no lock held.
//!
//! A page is what [`read`] finds from the asked offset `from`, in this order:
//!
//! 1. **The end.** For an ended job whose result reports the stream's length: at the length,
//!    the page is empty and at the end (`eof`); at or past the offset where the bytes the
//!    host could not keep begin (the length minus the dropped bytes), the rest of the stream
//!    is one gap and the page is at the end. Neither reads the store.
//! 2. **The final file.** For an ended job (with no dropped bytes, which have no final), the
//!    final `P/<stream>.log` serves the page by a ranged read when it is there: it holds the
//!    whole stream, so the page is at the end when it reaches the object's length.
//! 3. **The chunks and gap markers.** A listing of the stream's keys from `from` on (one page,
//!    [`MAX_LIST_KEYS`] keys) finds the objects at and after `from`. When none begins exactly
//!    at `from`, the object that may hold it begins before it, at most `MAX_CHUNK_BYTES` before
//!    (no chunk is longer): a second listing from there, continued for at most
//!    [`HOLDER_PAGES`] pages, finds the last object that begins before `from`. A marker
//!    covering the position is skipped as a gap (its body says its length); a hole, a range no
//!    object covers, is skipped as a gap once it is settled (below) and otherwise ends the
//!    page; a chunk covering the position gives the page's data, followed by the chunks
//!    exactly contiguous with it, up to the page's size and at most [`PAGE_CHUNKS`] chunks.
//!    Data ends at a marker or a hole, so the next page reports it. A page that ends at the end
//!    of the first listing's keys ends there, and the next call lists from its end.
//!
//! **A limit.** When more than [`HOLDER_PAGES`] pages of keys begin in the MiB before an
//! offset inside a chunk, the reader cannot find that chunk and answers an empty page there.
//! An offset a page ended at is never inside a chunk unless the page's size cut it, and
//! [`PAGE_CHUNKS`] keeps a page of small chunks ending on a chunk's end.
//!
//! Every gap a page skips lies between `from` and the page's offset, so a page's gaps are
//! the one range `[from, offset)`.
//!
//! **Settled.** A hole may still be filled while an upload is in flight or the store's
//! listing lags, so it is reported only once the object after it was last written
//! [`LOG_SETTLE`] before the read, or the job ended that long before. A hole at the end of an
//! ended job's stream (below its reported length) is reported once the job ended
//! [`LOG_SETTLE`] ago; for an ended job with no reported length (lost, or failed to start)
//! the page is then at the end where the objects end.
//!
//! **A chunk deleted under the reader.** The agent writes the final and then deletes the
//! chunks, so a chunk listed a moment ago can be gone when it is read. For an ended job a
//! chunk that is gone, or a hole about to be reported, first sends the reader back to the
//! final once; only a final still absent leaves it to the rules above.
//!
//! **A reused job id.** Objects last written more than [`REUSE_MARGIN`] before the job was
//! placed belong to an earlier job that held the same id, and are not read. The margin
//! covers the store's clock running behind the server's and timestamps in whole seconds; an
//! earlier job's objects are older by at least its retention window.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use js_logstore::layout::{FIRST_BYTE_AT_MS, decode_gap, parse_chunk_name};
use js_logstore::{
    HttpRequest, HttpResponse, JobPrefix, MAX_CHUNK_BYTES, MAX_GET_BYTES, MAX_LIST_KEYS, Store,
    StoreError, Stream, Transport, TransportError,
};

use crate::logs::StoreClock;

/// How long a hole may still be filled: an upload in flight, or a listing that lags.
pub(crate) const LOG_SETTLE: Duration = Duration::from_secs(30);

/// How long before a job's placement its own objects may look written: the store's clock
/// behind the server's, and timestamps in whole seconds.
pub(crate) const REUSE_MARGIN: Duration = Duration::from_secs(300);

/// The most chunks one page reads: one ranged read each.
pub(crate) const PAGE_CHUNKS: usize = 64;

/// The most pages of keys listed to find the object that holds an offset inside it.
pub(crate) const HOLDER_PAGES: usize = 4;

/// The most bytes of a gap marker's body read: `{"length":N}` is at most 31.
const MARKER_BYTES: u64 = 64;

/// [`MAX_CHUNK_BYTES`] as a stream offset.
const CHUNK: u64 = MAX_CHUNK_BYTES as u64;

/// The store client the reader reads with: one bucket, the read key, the server's clock.
pub(crate) type LogStore = Store<Port, StoreClock>;

/// A transport shared behind an [`Arc`]: the production one, or a test's fake.
#[derive(Clone)]
pub(crate) struct Port(pub(crate) Arc<dyn Transport>);

impl Transport for Port {
    fn send(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        self.0.send(request)
    }
}

/// How an ended job's stream ends, as its record says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct End {
    /// When the job ended, in milliseconds since the Unix epoch.
    pub(crate) at_ms: u64,
    /// The stream's length; `None` when no result reported it (a lost job).
    pub(crate) length: Option<u64>,
    /// Where the bytes the host could not keep begin, when it dropped any: the length minus
    /// the dropped bytes. Nothing at or after it is in the store.
    pub(crate) kept: Option<u64>,
}

/// What one page is read for.
#[derive(Debug, Clone)]
pub(crate) struct Ask {
    /// The job's prefix `P`.
    pub(crate) prefix: JobPrefix,
    /// The stream.
    pub(crate) stream: Stream,
    /// The offset of the first byte wanted.
    pub(crate) from: u64,
    /// The most bytes of data: 1 to [`MAX_GET_BYTES`].
    pub(crate) max: u64,
    /// The server's clock, in milliseconds since the Unix epoch.
    pub(crate) now_ms: u64,
    /// Objects last written before this instant (milliseconds) belong to an earlier job.
    pub(crate) cutoff_ms: u64,
    /// How the stream ends; `None` while the job has not ended.
    pub(crate) end: Option<End>,
}

impl Ask {
    /// The offset past which the store holds nothing of the stream.
    fn limit(&self) -> u64 {
        self.end
            .and_then(|end| end.kept.or(end.length))
            .unwrap_or(u64::MAX)
    }

    /// Whether the job ended [`LOG_SETTLE`] ago.
    fn end_settled(&self) -> bool {
        self.end.is_some_and(|end| settled(end.at_ms, self.now_ms))
    }
}

/// One page of a stream.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Page {
    /// The offset of the first byte of `data`; the range from the asked offset to it is a gap.
    pub(crate) offset: u64,
    /// Contiguous bytes of the stream from `offset`, as stored.
    pub(crate) data: Vec<u8>,
    /// The job has ended and the page reaches the end of the stream.
    pub(crate) eof: bool,
    /// The first-byte time of the chunk `data` begins in; `None` from a final or for no data.
    pub(crate) first_byte_at_ms: Option<u64>,
}

impl Page {
    /// Where the next page starts: the offset plus the data.
    pub(crate) fn next_offset(&self) -> u64 {
        self.offset.saturating_add(len(&self.data))
    }

    /// Whether the page of a job that has not ended says nothing new about the stream after
    /// `from`: no gap and no data (such a page is never at the end).
    pub(crate) fn is_empty_after(&self, from: u64) -> bool {
        self.offset == from && self.data.is_empty()
    }
}

/// One object of the listing: a chunk or a gap marker, covering `[offset, end)`.
#[derive(Debug)]
struct Object {
    key: String,
    offset: u64,
    end: u64,
    is_gap: bool,
    written_ms: u64,
}

/// What the listing found at and after the asked offset.
struct Listing {
    objects: Vec<Object>,
    /// More keys follow the last one listed.
    truncated: bool,
}

/// The page `ask` reads from `store`.
///
/// # Errors
///
/// What the store answered, when a request failed.
pub(crate) fn read(store: &LogStore, ask: &Ask) -> Result<Page, StoreError> {
    let mut walk = Walk {
        store,
        ask,
        pos: ask.from,
        finals: 2,
    };
    if let Some(page) = walk.at_end() {
        return Ok(page);
    }
    if let Some(page) = walk.from_final()? {
        return Ok(page);
    }
    match list(store, ask)? {
        Some(listing) => walk.through(listing),
        None => Ok(walk.page(false)),
    }
}

/// A read in progress: the position reached, every byte before it a gap.
struct Walk<'a> {
    store: &'a LogStore,
    ask: &'a Ask,
    pos: u64,
    /// How many more times the final may be asked for: first, and once more before a chunk
    /// that is gone or a hole is taken as missing.
    finals: u8,
}

impl Walk<'_> {
    /// The page so far, with no data.
    fn page(&self, eof: bool) -> Page {
        Page {
            offset: self.pos,
            eof,
            ..Page::default()
        }
    }

    /// Skips to `to`: the bytes between are a gap.
    fn skip_to(&mut self, to: u64) {
        self.pos = self.pos.max(to.min(self.ask.limit()));
    }

    /// The page at the end of an ended job's stream, when the position is there: at its
    /// length, or at or past where its dropped bytes begin (they are a gap to the length).
    fn at_end(&mut self) -> Option<Page> {
        let end = self.ask.end?;
        let length = end.length?;
        if end.kept.is_some_and(|kept| self.pos >= kept) {
            self.pos = length;
        }
        (self.pos == length).then(|| self.page(true))
    }

    /// The page the final serves from the position, when the job has ended, the final may
    /// still be asked for, and it is there.
    fn from_final(&mut self) -> Result<Option<Page>, StoreError> {
        let wanted = self.finals > 0 && self.ask.end.is_some_and(|end| end.kept.is_none());
        self.finals = self.finals.saturating_sub(1);
        if !wanted {
            return Ok(None);
        }
        let key = self.ask.prefix.final_key(self.ask.stream);
        let listed = self.store.list(&key, None, 1)?;
        let Some(entry) = listed.entries.into_iter().find(|entry| entry.key == key) else {
            return Ok(None);
        };
        if millis(entry.last_modified) < self.ask.cutoff_ms {
            return Ok(None);
        }
        let room = entry.size.saturating_sub(self.pos);
        let want = self.ask.max.min(room).min(MAX_GET_BYTES);
        if want == 0 {
            return Ok(Some(self.page(true)));
        }
        let Some(object) = self.store.get_range(&key, self.pos, want)? else {
            return Ok(None);
        };
        let offset = self.pos;
        let next = offset.saturating_add(len(&object.bytes));
        Ok(Some(Page {
            offset,
            eof: next >= object.total_len,
            data: object.bytes,
            first_byte_at_ms: None,
        }))
    }

    /// The page the listing's objects give from the position.
    fn through(mut self, mut listing: Listing) -> Result<Page, StoreError> {
        let mut at = 0;
        loop {
            if let Some(page) = self.at_end() {
                return Ok(page);
            }
            while listing
                .objects
                .get(at)
                .is_some_and(|object| object.end <= self.pos)
            {
                at += 1;
            }
            let Some(object) = listing.objects.get(at) else {
                return self.past_the_objects(listing.truncated);
            };
            if object.offset > self.pos {
                let (to, written_ms) = (object.offset, object.written_ms);
                if !(settled(written_ms, self.ask.now_ms) || self.ask.end_settled()) {
                    return Ok(self.page(false));
                }
                if let Some(page) = self.from_final()? {
                    return Ok(page);
                }
                self.skip_to(to);
            } else if object.is_gap {
                let to = object.end;
                self.skip_to(to);
                at += 1;
            } else {
                if let Some(page) = self.data(&listing.objects[at..])? {
                    return Ok(page);
                }
                if let Some(page) = self.from_final()? {
                    return Ok(page);
                }
                listing.objects.remove(at);
            }
        }
    }

    /// The page when no listed object is at or after the position.
    fn past_the_objects(mut self, truncated: bool) -> Result<Page, StoreError> {
        if truncated || !self.ask.end_settled() {
            return Ok(self.page(false));
        }
        if let Some(page) = self.from_final()? {
            return Ok(page);
        }
        // Settled and past every object: the rest up to the reported length is a gap, and
        // with no reported length the stream ends where its objects end.
        if let Some(length) = self.ask.end.and_then(|end| end.length) {
            self.pos = length;
        }
        Ok(self.page(true))
    }

    /// The data from the first of `objects`, a chunk covering the position, and the chunks
    /// exactly contiguous with it; `None` when that first chunk is gone.
    fn data(&mut self, objects: &[Object]) -> Result<Option<Page>, StoreError> {
        let offset = self.pos;
        let limit = self.ask.limit();
        let mut data = Vec::new();
        let mut first_byte_at_ms = None;
        for (index, object) in objects.iter().enumerate() {
            let room = self.ask.max - len(&data);
            let contiguous = index == 0 || object.offset == self.pos;
            if object.is_gap || !contiguous || room == 0 || index == PAGE_CHUNKS {
                break;
            }
            let want = room
                .min(object.end - self.pos)
                .min(limit - self.pos)
                .min(MAX_GET_BYTES);
            let read = self
                .store
                .get_range(&object.key, self.pos - object.offset, want);
            let object = match read {
                Ok(Some(object)) => object,
                Ok(None) | Err(StoreError::OutOfRange) if index == 0 => return Ok(None),
                Ok(None) | Err(StoreError::OutOfRange) => break,
                Err(error) => return Err(error),
            };
            if index == 0 {
                first_byte_at_ms = object
                    .metadata
                    .get(FIRST_BYTE_AT_MS)
                    .and_then(|value| value.parse().ok());
            }
            let got = len(&object.bytes);
            self.pos += got;
            data.extend(object.bytes);
            if got < want {
                break;
            }
        }
        let eof = self.ask.end.and_then(|end| end.length) == Some(self.pos);
        Ok(Some(Page {
            offset,
            data,
            eof,
            first_byte_at_ms,
        }))
    }
}

/// The chunks and gap markers of the stream that may hold `ask.from` and those after it, in
/// offset order; `None` when the one that may hold it could not be found ([`HOLDER_PAGES`]).
fn list(store: &LogStore, ask: &Ask) -> Result<Option<Listing>, StoreError> {
    let prefix = ask.prefix.stream_prefix(ask.stream);
    let after = ask
        .from
        .checked_sub(1)
        .map(|offset| ask.prefix.chunk_key(ask.stream, offset));
    let page = store.list(&prefix, after.as_deref(), MAX_LIST_KEYS)?;
    let listed: Vec<Named> = page
        .entries
        .into_iter()
        .filter_map(|entry| named(ask, &prefix, entry))
        .filter(|object| object.offset >= ask.from)
        .collect();
    let mut found = Vec::new();
    if ask.from > 0 && listed.first().is_none_or(|first| first.offset != ask.from) {
        match holder(store, ask, &prefix)? {
            Holder::Unknown => return Ok(None),
            Holder::Found(object) => found.extend(object),
        }
    }
    found.extend(listed);
    let mut objects = Vec::new();
    for object in found {
        if let Some(object) = resolved(store, object)? {
            objects.push(object);
        }
    }
    Ok(Some(Listing {
        objects,
        truncated: page.truncated,
    }))
}

/// What the search for the object before `ask.from` found.
enum Holder {
    /// The last object that begins before `ask.from`, if any.
    Found(Option<Named>),
    /// More than [`HOLDER_PAGES`] pages of keys begin before `ask.from`.
    Unknown,
}

/// The last object of the stream that begins before `ask.from`, listed from the key of
/// `ask.from - MAX_CHUNK_BYTES` on: the only one that may cover `ask.from`, since objects
/// never overlap.
fn holder(store: &LogStore, ask: &Ask, prefix: &str) -> Result<Holder, StoreError> {
    let mut after = ask
        .from
        .checked_sub(CHUNK)
        .map(|offset| ask.prefix.chunk_key(ask.stream, offset));
    let mut last = None;
    for _ in 0..HOLDER_PAGES {
        let page = store.list(prefix, after.as_deref(), MAX_LIST_KEYS)?;
        after = page.entries.last().map(|entry| entry.key.clone());
        for object in page
            .entries
            .into_iter()
            .filter_map(|e| named(ask, prefix, e))
        {
            if object.offset >= ask.from {
                return Ok(Holder::Found(last));
            }
            last = Some(object);
        }
        if !page.truncated {
            return Ok(Holder::Found(last));
        }
    }
    Ok(Holder::Unknown)
}

/// A listed key the layout names, not of an earlier job and not past the stream's limit.
struct Named {
    key: String,
    offset: u64,
    size: u64,
    is_gap: bool,
    written_ms: u64,
}

/// What `entry`, listed under `prefix`, is for `ask`; `None` for a name that is not the
/// layout's, an object of an earlier job, or one past the stream's limit.
fn named(ask: &Ask, prefix: &str, entry: js_logstore::Entry) -> Option<Named> {
    let name = entry.key.strip_prefix(prefix).and_then(parse_chunk_name)?;
    let written_ms = millis(entry.last_modified);
    let kept = name.offset < ask.limit() && written_ms >= ask.cutoff_ms;
    kept.then_some(Named {
        key: entry.key,
        offset: name.offset,
        size: entry.size,
        is_gap: name.is_gap,
        written_ms,
    })
}

/// The object `named` is, with its end: a chunk's from its size, a marker's from its body;
/// `None` for an empty chunk, and for a marker that is gone or whose body is not a marker's.
fn resolved(store: &LogStore, named: Named) -> Result<Option<Object>, StoreError> {
    let end = match named.is_gap {
        true => marker_end(store, &named.key, named.offset)?,
        false => Some(named.offset.saturating_add(named.size)).filter(|_| named.size > 0),
    };
    Ok(end.map(|end| Object {
        key: named.key,
        offset: named.offset,
        end,
        is_gap: named.is_gap,
        written_ms: named.written_ms,
    }))
}

/// Where the gap the marker `key` at `offset` describes ends; `None` when the marker is gone
/// or its body is not a marker's.
fn marker_end(store: &LogStore, key: &str, offset: u64) -> Result<Option<u64>, StoreError> {
    let body = match store.get_range(key, 0, MARKER_BYTES) {
        Ok(Some(object)) => object.bytes,
        Ok(None) | Err(StoreError::OutOfRange) => return Ok(None),
        Err(error) => return Err(error),
    };
    let gap = decode_gap(offset, &body).ok();
    Ok(gap.map(|gap| gap.offset().saturating_add(gap.length())))
}

/// Whether an instant `at_ms` is [`LOG_SETTLE`] before `now_ms`.
fn settled(at_ms: u64, now_ms: u64) -> bool {
    at_ms.saturating_add(duration_millis(LOG_SETTLE)) <= now_ms
}

/// `time` in milliseconds since the Unix epoch; 0 before it.
fn millis(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH).map_or(0, duration_millis)
}

/// `duration` in whole milliseconds, the largest value when it does not fit.
pub(crate) fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// The length of `bytes` as a stream offset.
fn len(bytes: &[u8]) -> u64 {
    u64::try_from(bytes.len()).unwrap_or(u64::MAX)
}
