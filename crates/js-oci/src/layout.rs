//! An OCI image-layout directory (`oci-layout`, `index.json`, `blobs/sha256/<hex>`) as a
//! [`BundleSource`]: the second place an artifact can be read from, for a host with no
//! registry and for tests.
//!
//! The directory is data that was written by someone else, so it is read like a hostile
//! one. What is refused here and not by a registry:
//! - a symlink anywhere below the root on the way to a file this client reads (so
//!   `blobs/sha256` pointing elsewhere, or a blob that is a link to a file outside the
//!   layout, cannot make it read what the layout does not hold); the file itself is
//!   opened with `O_NOFOLLOW`, and each directory above it is checked to be a directory
//!   and not a link. A process that can rewrite directories of the layout while it is
//!   read is outside this check: the layout is as trusted as the process that unpacked
//!   it;
//! - an `oci-layout` that does not say version `1.0.0`;
//! - a blob that is absent;
//! - a manifest that `index.json` does not list.
//!
//! What a registry also refuses is refused by the same code: the manifest's digest, an
//! index, the kind's types and limits ([`crate::manifest`]), and each blob's length and
//! digest while it is read ([`crate::verify`]). A blob whose bytes do not hash to its
//! name is a [`Error::BlobDigest`], exactly as a registry that serves it would give.

use std::io;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::Deserialize;
use tokio::fs::{File, OpenOptions};
use tokio::io::AsyncReadExt as _;

use crate::client::BlobSink;
use crate::manifest::{self, Descriptor, Kind, Manifest};
use crate::source::BundleSource;
use crate::text::{escape, shown};
use crate::verify::Receiving;
use crate::{Digest, Error};

/// The only layout version this client reads.
const VERSION: &str = "1.0.0";
/// The most bytes of `oci-layout` read: it holds one short string.
const MARKER_LIMIT: u64 = 4096;
/// How much of a blob is read at a time.
const CHUNK: usize = 64 << 10;

/// Why a directory is not an image layout this client reads. Paths are relative to the
/// layout's root.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LayoutFault {
    /// `oci-layout` states a version other than `1.0.0`.
    #[error("oci-layout declares version {0}, and only version 1.0.0 is read")]
    Version(String),
    /// `oci-layout` or `index.json` is not the JSON it should be.
    #[error("{file} is not what a layout holds: {why}")]
    Malformed {
        /// Which file.
        file: &'static str,
        /// What is wrong with it.
        why: String,
    },
    /// A file or directory that is absent or cannot be opened.
    #[error("{path} cannot be read: {source}")]
    Unreadable {
        /// Where.
        path: String,
        /// What the system said; its kind is `NotFound` for a blob that is missing.
        #[source]
        source: io::Error,
    },
    /// A symlink where a layout holds a file or directory.
    #[error("{0} is a symlink, and a layout is read without following one")]
    Symlink(String),
    /// A file where a directory belongs, or a directory or special file where a file
    /// belongs.
    #[error("{0} is not the kind of file a layout holds there")]
    NotRegular(String),
    /// The manifest pinned is not one `index.json` lists.
    #[error("the manifest {0} is not listed in index.json")]
    NotListed(String),
}

/// An OCI image-layout directory, read as a [`BundleSource`]. Opening it checks
/// `oci-layout`; nothing else is read until a fetch asks for it.
#[derive(Debug)]
pub struct Layout {
    root: PathBuf,
}

impl Layout {
    /// The layout at `root`. The root itself may be reached through a link; only what is
    /// below it is not followed.
    ///
    /// # Errors
    /// [`LayoutFault`] (as [`Error::Layout`]) when `oci-layout` is absent, not a file,
    /// not JSON, or states a version other than `1.0.0`.
    pub async fn open(root: impl Into<PathBuf>) -> Result<Self, Error> {
        #[derive(Deserialize)]
        struct Marker {
            #[serde(rename = "imageLayoutVersion")]
            version: String,
        }
        let layout = Self { root: root.into() };
        let bytes = layout.read(&["oci-layout"], MARKER_LIMIT).await?;
        let marker: Marker = serde_json::from_slice(&bytes).map_err(malformed("oci-layout"))?;
        if marker.version != VERSION {
            return Err(Error::Layout(LayoutFault::Version(shown(&marker.version))));
        }
        Ok(layout)
    }

    /// Opens the file `parts` names below the root, for reading, refusing a link or a
    /// directory above it and a link, directory or special file at the end.
    async fn open_file(&self, parts: &[&str]) -> Result<File, Error> {
        let (dirs, last) = parts.split_at(parts.len() - 1);
        let mut below = PathBuf::new();
        for dir in dirs {
            below.push(dir);
            let meta = tokio::fs::symlink_metadata(self.root.join(&below))
                .await
                .map_err(unreadable(&below))?;
            if meta.is_symlink() {
                return Err(fault_at(&below, LayoutFault::Symlink));
            }
            if !meta.is_dir() {
                return Err(fault_at(&below, LayoutFault::NotRegular));
            }
        }
        below.push(last[0]);
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.root.join(&below))
            .await
            .map_err(unreadable(&below))?;
        if !file.metadata().await.is_ok_and(|meta| meta.is_file()) {
            return Err(fault_at(&below, LayoutFault::NotRegular));
        }
        Ok(file)
    }

    /// The whole file `parts` names, at most `limit` bytes.
    async fn read(&self, parts: &[&str], limit: u64) -> Result<Vec<u8>, Error> {
        let file = self.open_file(parts).await?;
        let mut bytes = Vec::new();
        file.take(limit + 1)
            .read_to_end(&mut bytes)
            .await
            .map_err(unreadable(&parts.join("/")))?;
        if bytes.len() as u64 > limit {
            return Err(Error::TooLarge {
                what: parts.join("/"),
                limit,
            });
        }
        Ok(bytes)
    }
}

/// The error for `source` failing to open or read `at`: a link is its own fault.
fn unreadable(at: impl AsRef<Path>) -> impl FnOnce(io::Error) -> Error {
    let at = at.as_ref().display().to_string();
    move |source| {
        if source.raw_os_error() == Some(libc::ELOOP) {
            Error::Layout(LayoutFault::Symlink(at))
        } else {
            Error::Layout(LayoutFault::Unreadable { path: at, source })
        }
    }
}

/// `make` applied to `at`, as an error.
fn fault_at(at: &Path, make: fn(String) -> LayoutFault) -> Error {
    Error::Layout(make(at.display().to_string()))
}

/// The error for `file` not being JSON.
fn malformed(file: &'static str) -> impl FnOnce(serde_json::Error) -> Error {
    move |e| {
        Error::Layout(LayoutFault::Malformed {
            file,
            why: escape(&e.to_string()),
        })
    }
}

#[async_trait]
impl BundleSource for Layout {
    async fn fetch_manifest(&self, digest: &Digest, kind: &Kind) -> Result<Manifest, Error> {
        #[derive(Deserialize)]
        struct Index {
            manifests: Vec<Entry>,
        }
        #[derive(Deserialize)]
        struct Entry {
            digest: String,
        }
        let limit = kind.limits.manifest;
        let index = self.read(&["index.json"], limit).await?;
        let index: Index = serde_json::from_slice(&index).map_err(malformed("index.json"))?;
        if !index.manifests.iter().any(|m| m.digest == digest.as_str()) {
            return Err(Error::Layout(LayoutFault::NotListed(digest.to_string())));
        }
        let bytes = self.read(&["blobs", "sha256", digest.hex()], limit).await?;
        manifest::parse(&bytes, digest, kind)
    }

    async fn fetch_blob(&self, blob: &Descriptor, sink: &mut dyn BlobSink) -> Result<(), Error> {
        let parts = ["blobs", "sha256", blob.digest().hex()];
        let mut file = self.open_file(&parts).await?;
        let mut receiving = Receiving::new(blob);
        let mut buf = vec![0u8; CHUNK];
        loop {
            let n = file
                .read(&mut buf)
                .await
                .map_err(unreadable(parts.join("/")))?;
            if n == 0 {
                break;
            }
            receiving.accept(&buf[..n], sink).await?;
        }
        receiving.finish(sink).await
    }
}
