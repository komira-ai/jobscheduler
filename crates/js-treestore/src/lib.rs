//! The tree store: sealed, content-addressed trees extracted from layer blobs.
//!
//! A layer outside the host blob store. It pins a layer blob through [`BlobStore`], extracts
//! it with `js_bundle::unpack_layer`, seals the stage through a [`Sealer`], renames it to
//! `trees/<diff id hex>`, writes a record beside it and registers the tree's bytes with the
//! blob store (`account`), so they share its one budget. The blob store asks for the bytes
//! back in its eviction order through [`Release`]; a pinned or kept tree answers `Busy`.
//!
//! Layout under the directory: `trees/<hex>` and `trees/<hex>.sealed` (the record),
//! `stage/` (extractions in flight), `trash/` (trees renamed for deletion), `pins/<hex>/<run>`
//! (an empty file per run that uses the tree; durable, so a crash cannot lose one).
//!
//! Crash safety is stage, rename, record: a tree counts as present only with a current
//! record. [`TreeStore::open`] clears `stage/` and `trash/`, trashes a tree with no valid
//! record, drops pins of runs that are not live and registers the bytes of every tree, and
//! the host calls the blob store's `reap_pins` and `start_gc` after it.
//!
//! [`Release`]: blobstore::Release

pub mod blobstore;
mod error;
mod record;
mod sealer;
mod store;

pub use blobstore::BlobStore;
pub use error::{Error, Step};
pub use sealer::{SealError, Sealed, Sealer};
pub use store::{
    Config, EXTRACTOR_VERSION, Ready, RunId, TREE_PRIORITY, TreeHandle, TreeStats, TreeStore,
    TreeWanted,
};
