//! The pack format a build's outputs are stored in, and the upload grant and report
//! that move packs between a frontend and a host.
//!
//! A pack is many blobs in one stored object: records back to back, then a fan-out, an
//! index and a trailer ([`layout`] has the bytes). The host side writes packs
//! ([`PackWriter`]); the frontend side checks an untrusted host's pack against its
//! report ([`verify_pack`]) before it adopts the object in place. Both use this one
//! crate, so the writer and the verifier cannot drift.
//!
//! - [`PackWriter`] streams records to a sink and ends with the footer, within
//!   [`PackLimits`] (at most [`MAX_PACK_BYTES`] and [`MAX_BLOBS_PER_PACK`]; a blob of
//!   [`OWN_PACK_BLOB_BYTES`] or more has a pack of its own).
//! - [`Footer`] reads a footer from the last bytes of an object; [`verify_pack`] is the
//!   strict check; [`PackReader`] finds a blob in a whole pack held in memory.
//! - [`UploadGrant`], [`UploadSlot`], [`PackReport`], [`PackEntry`] and
//!   [`UploadOutcome`] are the wire types. An [`UploadGrant`] is a secret: its `Debug`
//!   hides every URL.
//!
//! # Origin
//!
//! The byte format, the writer and the footer reader are moved from kbf
//! (komira-ai/kbf) at 89af8f2efa86250552b386eacdfe2cd554f75deb: `crates/kbf-segments`
//! (`src/layout.rs`, `src/segment.rs`, `src/error.rs`, `src/crc32c.rs`), where a pack
//! is called a segment, and `docs/design/storage.md#segments`. The bytes are identical,
//! including the magic `KBFSEG\r\n`: a pack written by either side is read by the other.
//! The writer differs in API only: it streams records to a sink instead of holding them,
//! and it enforces the blob-count and own-pack limits. The strict verifier, the limits
//! and the wire types are new here.
//!
//! This is a pure crate: no async runtime, network, clock, randomness or hashed
//! collections.

#![deny(clippy::disallowed_methods, clippy::disallowed_types)]

mod digest;
mod error;
pub mod layout;
mod verify;
mod wire;
mod writer;

pub use crate::digest::Digest;
pub use crate::error::{PackError, WriteError};
pub use crate::layout::{
    Footer, IndexEntry, MAX_BLOBS_PER_PACK, MAX_PACK_BYTES, OWN_PACK_BLOB_BYTES, PackLimits,
};
pub use crate::verify::{PackReader, verify_pack};
pub use crate::wire::{PackEntry, PackReport, UploadGrant, UploadOutcome, UploadSlot};
pub use crate::writer::{Finished, PackWriter};
