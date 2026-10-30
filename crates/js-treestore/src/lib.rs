//! The tree store: sealed, content-addressed trees extracted from layer blobs.
//!
//! It owns its directory, its budget and its eviction. A layer blob comes from a
//! [`LayerSource`] into `tmp/` through a sink that hashes as it receives; size and digest
//! are checked before any byte is parsed; the layer is extracted with
//! `js_bundle::unpack_layer` into `stage/`, sealed through a [`Sealer`], renamed to
//! `trees/<diff id hex>` and recorded beside it (`<hex>.sealed`).
//!
//! Layout under the directory: `trees/`, `tmp/` (blobs being fetched), `stage/`
//! (extractions in flight), `trash/` (trees renamed for deletion) and `pins/<hex>/<run>`
//! (an empty file per run that uses a tree; durable, so a crash cannot lose one).
//!
//! Eviction: when a new tree would take the used bytes over the budget, or the volume's
//! free space under the low-water mark, the least recently used tree that no run pins, that
//! is not kept and that was not used within the minimum age is deleted, until there is room
//! or nothing is left to delete ([`Error::NoSpace`], retriable).
//!
//! Trees are named by their diff id alone. A tree that is present answers every caller of
//! that diff id, whatever blob digest or limits the caller gives: the content is the same by
//! the definition of the diff id (a tree extracted under lax limits answers a caller with
//! strict ones). The record names the blob the tree came from and the extractor version.
//!
//! One store holds a directory at a time (`lock`). Bytes in `tmp/` and `stage/` are not
//! counted in the budget or the free-space check. The record is not bound to the tree's
//! contents; a scrub that re-hashes trees is a later step.
//!
//! The [`mac`] module holds the macOS side of a [`Sealer`] (root-owned trees with `uchg`)
//! and the checks made before a store starts on a Mac: the volume and the securelevel.
//!
//! Crash safety is stage, rename, record: a tree counts as present only with a current
//! record. [`TreeStore::open`] clears `tmp/`, `stage/` and `trash/`, trashes a tree with no
//! valid record and drops pins of runs that are not live.

mod error;
pub mod mac;
mod record;
mod sealer;
mod source;
mod store;

pub use error::{Error, Step};
pub use sealer::{SealError, Sealed, Sealer};
pub use source::{BlobRef, BlobSink, Digest, FetchError, LabelError, LayerSource};
pub use store::{
    Config, EXTRACTOR_VERSION, Ready, RunId, TreeHandle, TreeStats, TreeStore, TreeWanted,
};
