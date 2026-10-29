//! The host blob store's public API, as this crate uses it.
//!
//! The store is a separate component; this module is the trait over the names of its
//! contract (`ensure`, `ensure_all`, `put`, `open_verified`, `release`, `stats`, `account`
//! and its handle, `Release` and `Busy`) so the tree store builds and is tested against a
//! fake before the store lands. The field and error names are the store's; the bodies are
//! its own. Calls this crate does not make (`ensure_all`, `put`, `stats`) are in the trait
//! so a store satisfies one contract.
//!
//! One adaptation: the store's `AccountHandle` is a struct, here a trait over a shared
//! handle, and `drop_entry` takes `&self` because the handle is shared with the tree
//! store's bookkeeping.

use std::fmt;
use std::io::Read;
use std::os::fd::OwnedFd;
use std::sync::Arc;

/// The digest algorithm of a blob key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Algorithm {
    /// SHA-256.
    Sha256,
}

/// The key of a blob: `sha256:` and 64 lowercase hex digits.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlobKey {
    /// The algorithm.
    pub alg: Algorithm,
    /// The 64 hex digits.
    pub hex: String,
}

/// A text that is not a blob key, holder or run id.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid {what}: {text:?}")]
pub struct LabelError {
    /// What was being parsed.
    pub what: &'static str,
    /// The text, as given.
    pub text: String,
}

impl BlobKey {
    /// Parses `sha256:<64 lowercase hex>`.
    ///
    /// # Errors
    /// The text is any other form.
    pub fn parse(text: &str) -> Result<BlobKey, LabelError> {
        let hex = text.strip_prefix("sha256:").unwrap_or("");
        let ok = hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        if ok {
            Ok(BlobKey {
                alg: Algorithm::Sha256,
                hex: hex.to_owned(),
            })
        } else {
            Err(LabelError {
                what: "blob key",
                text: text.to_owned(),
            })
        }
    }
}

impl fmt::Display for BlobKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "sha256:{}", self.hex)
    }
}

fn label_ok(text: &str) -> bool {
    let chars = text
        .bytes()
        .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'));
    chars && (1..=128).contains(&text.len())
}

/// Who holds a pin: an opaque label of `[a-z0-9._-]`, 1 to 128 bytes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Holder(String);

impl Holder {
    /// Validates a holder label.
    ///
    /// # Errors
    /// The label is empty, over 128 bytes or has another character.
    pub fn new(text: &str) -> Result<Holder, LabelError> {
        if label_ok(text) {
            Ok(Holder(text.to_owned()))
        } else {
            Err(LabelError {
                what: "holder",
                text: text.to_owned(),
            })
        }
    }

    /// The label.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A live pin on a blob. Dropping it does not release; release is by holder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pinned {
    /// The blob.
    pub key: BlobKey,
    /// Its size in bytes.
    pub size: u64,
}

/// The pins of one `ensure_all`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinSet {
    /// One pin per wanted blob.
    pub pins: Vec<Pinned>,
}

/// Where a blob comes from when the store does not hold it. The store defines the real
/// type; until it lands this is its label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source(pub String);

/// The lease a request is made for; it scopes the sharing of credentialed entries.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Principal(pub String);

/// What `ensure` is asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wanted {
    /// The blob.
    pub key: BlobKey,
    /// Its size, when the caller knows it.
    pub size: Option<u64>,
    /// The most bytes the store may fetch for it.
    pub max_size: u64,
    /// Where to fetch it from.
    pub source: Source,
    /// The lease the blob is fetched for.
    pub principal: Principal,
}

/// A failure of an upstream the store fetches from.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("upstream: {0}")]
pub struct UpstreamError(pub String);

/// The store's errors.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The budget cannot take the blob.
    #[error("no space: used {used}, pinned {pinned}, budget {budget}, incoming {incoming}")]
    NoSpace {
        /// Bytes used.
        used: u64,
        /// Bytes pinned.
        pinned: u64,
        /// The budget.
        budget: u64,
        /// The bytes asked for.
        incoming: u64,
    },
    /// The blob is over what may be held at all.
    #[error("blob too large")]
    TooLarge,
    /// A stored blob failed its check.
    #[error("blob {0} is corrupt")]
    Corrupt(BlobKey),
    /// A fetched blob is not the one asked for.
    #[error("digest mismatch: wanted {want}, got {got}")]
    Digest {
        /// The key asked for.
        want: BlobKey,
        /// The key of the bytes.
        got: BlobKey,
    },
    /// The blob is over `max_size`.
    #[error("blob over its size cap")]
    SizeCap,
    /// The caller may not hold the blob.
    #[error("not entitled")]
    Forbidden,
    /// A release callback called `account`, `ensure*`, `put` or `release`.
    #[error("called from inside a release callback")]
    Reentrant,
    /// The upstream failed.
    #[error(transparent)]
    Upstream(#[from] UpstreamError),
    /// A file-system call failed.
    #[error("store i/o: {0}")]
    Io(#[from] std::io::Error),
    /// The store is not open for this yet.
    #[error("store not ready")]
    NotReady,
}

/// A blob opened after its digest was checked.
#[derive(Debug)]
pub struct VerifiedBlob {
    /// The open file.
    pub fd: OwnedFd,
    /// Its size.
    pub size: u64,
}

/// The store's counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stats {
    /// Bytes of blobs held.
    pub used: u64,
    /// Bytes pinned.
    pub pinned: u64,
    /// Bytes reserved for fetches in flight.
    pub reserved: u64,
    /// Bytes registered through `account`.
    pub accounted: u64,
    /// Free bytes on the volume.
    pub free: u64,
}

/// An owner's answer when its bytes are in use and cannot be released now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("in use")]
pub struct Busy;

/// What the store calls, in its one eviction order, to ask an owner to free its bytes.
pub trait Release: Send + Sync {
    /// Frees the bytes, returning how many, or answers `Busy`. Called outside the store's
    /// lock with a 30 s bound. It may call `update` and `drop_entry` on its own handle;
    /// `account`, `ensure*`, `put` and `release` return [`StoreError::Reentrant`].
    ///
    /// # Errors
    /// `Busy` while the owner uses the bytes.
    fn release(&self) -> Result<u64, Busy>;
}

/// The owner's handle on bytes it registered with `account`.
pub trait AccountHandle: Send + Sync {
    /// The new `st_blocks` total of the entry.
    fn update(&self, bytes: u64);
    /// Marks the entry as used now, for the recency order.
    fn touch(&self);
    /// The owner removed its bytes itself. An entry is removed once, by whichever of this
    /// and a release that returned `Ok` comes first.
    fn drop_entry(&self);
}

/// The host blob store, as the tree store uses it.
pub trait BlobStore: Send + Sync + 'static {
    /// A verified hit or a fetch through `want.source`, pinned for `holder`.
    ///
    /// # Errors
    /// As [`StoreError`].
    fn ensure(&self, want: &Wanted, holder: &Holder) -> Result<Pinned, StoreError>;
    /// All of `wants` or none.
    ///
    /// # Errors
    /// As [`StoreError`].
    fn ensure_all(&self, wants: &[Wanted], holder: &Holder) -> Result<PinSet, StoreError>;
    /// Stores the bytes of `r` as the blob `key`.
    ///
    /// # Errors
    /// As [`StoreError`].
    fn put(
        &self,
        key: &BlobKey,
        size: u64,
        r: &mut dyn Read,
        holder: &Holder,
        principal: &Principal,
    ) -> Result<Pinned, StoreError>;
    /// Opens a pinned blob, checking its digest first.
    ///
    /// # Errors
    /// As [`StoreError`].
    fn open_verified(&self, pin: &Pinned) -> Result<VerifiedBlob, StoreError>;
    /// Drops every pin the holder has.
    ///
    /// # Errors
    /// As [`StoreError`].
    fn release(&self, holder: &Holder) -> Result<(), StoreError>;
    /// A lease ended: prunes it from the admission sets.
    fn end_principal(&self, principal: &Principal);
    /// The counters.
    fn stats(&self) -> Stats;
    /// Counts `bytes` the caller owns on the store's volume in the one budget. Under
    /// pressure the store calls `r` in order of lower `priority` first, then least
    /// recently used; blobs are priority 0.
    ///
    /// # Errors
    /// As [`StoreError`].
    fn account(
        &self,
        holder: &Holder,
        ns: &str,
        bytes: u64,
        priority: u8,
        r: Arc<dyn Release>,
    ) -> Result<Arc<dyn AccountHandle>, StoreError>;
    /// Turns eviction on; called after every owner has registered its bytes.
    fn start_gc(&self);
}
