//! The persisted file: one TOML document, replaced whole on every change.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{OpenError, SaveError};
use crate::state::LeaseState;

/// The version this build writes. Readers ignore the number and the fields they do
/// not know, so a later version may add fields and an older build still loads it. A
/// build keeps what it does not know and writes it back, with the version it read when
/// that is higher, so an older build in between does not destroy a later one's data.
pub(crate) const FORMAT_VERSION: u32 = 1;

/// The largest integer a TOML file holds (`i64::MAX`). A term or a sequence above it
/// cannot be written, so the last id of a term or of the sequence is the one whose
/// successor is this value.
pub(crate) const LARGEST: u64 = i64::MAX.unsigned_abs();

/// The whole file.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Document {
    pub version: u32,
    pub term: u64,
    pub next_sequence: u64,
    /// How many `[[lease]]` tables follow. A file cut short (a partial restore) has
    /// fewer and is refused instead of loading as a smaller table.
    pub lease_count: u64,
    /// How many `[[reservation]]` tables follow; absent (written only when above zero)
    /// is none. Checked like `lease_count`: a file cut short is refused.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub reservation_count: u64,
    #[serde(default)]
    pub lease: Vec<StoredLease>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reservation: Vec<StoredReservation>,
    /// Fields of a later version, kept as read.
    #[serde(flatten)]
    pub extra: toml::Table,
}

fn is_zero(count: &u64) -> bool {
    *count == 0
}

/// One reservation record as written: the held part (`host`, `reserved_at_ms`,
/// `deadline_ms`, all three or none), the counters and the exclusions.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct StoredReservation {
    pub request_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reserved_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_ms: Option<u64>,
    #[serde(default)]
    pub give_ups: u32,
    #[serde(default)]
    pub prepare_failures: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclusion: Vec<StoredExclusion>,
    /// Fields of a later version, kept as read.
    #[serde(flatten)]
    pub extra: toml::Table,
}

/// One excluded host: until `until_ms`, or for good when `forever` is set.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct StoredExclusion {
    pub host: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub forever: bool,
}

/// One lease as written.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct StoredLease {
    pub term: u64,
    pub sequence: u64,
    pub state: LeaseState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    pub acquired_at_ms: u64,
    pub expires_at_ms: u64,
    pub ends_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation_run: Option<String>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    /// Whether the lease is of a whole-node class, whose ttl and maximum wall time
    /// count from the moment it is observed ready. Absent in a file written before the
    /// field existed, which loads as not whole.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub whole: bool,
    /// The ttl granted at acquire, kept while a whole lease is pending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_ms: Option<u64>,
    /// The maximum wall time of the request, kept while a whole lease is pending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wall_ms: Option<u64>,
    /// Fields of a later version, kept as read.
    #[serde(flatten)]
    pub extra: toml::Table,
}

/// Takes the exclusive lock of the table at `path`: a sibling `.lock` file, locked for
/// as long as the returned file lives. A second table over the same path fails here
/// with a `WouldBlock` error instead of issuing the same ids.
pub(crate) fn lock(path: &Path) -> Result<fs::File, OpenError> {
    let lock = sibling(path, ".lock");
    let mut options = fs::OpenOptions::new();
    options.create(true).truncate(false).write(true);
    let locked = options
        .open(&lock)
        .and_then(|file| file.try_lock().map(|()| file).map_err(io::Error::from));
    locked.map_err(|source| OpenError::Lock { path: lock, source })
}

/// Reads the document at `path`; `None` when there is no file.
pub(crate) fn read(path: &Path) -> Result<Option<Document>, OpenError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(OpenError::Read {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let parsed = toml::from_str::<Document>(&text);
    let document = parsed.map_err(|err| corrupt(path, err.to_string()))?;
    Ok(Some(document))
}

/// The error for a file that read but does not hold a valid table.
pub(crate) fn corrupt(path: &Path, reason: String) -> OpenError {
    OpenError::Corrupt {
        path: path.to_path_buf(),
        reason,
    }
}

/// Writes `document` to `path` atomically: a temporary file next to it is written and
/// fsynced, renamed over `path`, and the directory is fsynced, so a reader sees the
/// old file or the new one and a crash loses neither.
pub(crate) fn write(path: &Path, document: &Document) -> Result<(), SaveError> {
    let text = toml::to_string(document).map_err(io::Error::other);
    let written = text.and_then(|text| replace(path, text.as_bytes()));
    written.map_err(|source| SaveError {
        path: path.to_path_buf(),
        source,
    })
}

fn replace(path: &Path, bytes: &[u8]) -> io::Result<()> {
    replace_with(path, bytes, |from, to| fs::rename(from, to))
}

/// [`replace`] with the rename step passed in, so a test can make exactly that step fail
/// and see that the old file is still what a reader gets.
fn replace_with(
    path: &Path,
    bytes: &[u8],
    rename: impl FnOnce(&Path, &Path) -> io::Result<()>,
) -> io::Result<()> {
    let temp = sibling(path, ".tmp");
    let dir = path.parent().filter(|dir| !dir.as_os_str().is_empty());
    let dir = dir.unwrap_or(Path::new("."));
    fs::File::create(&temp)
        .and_then(|mut file| file.write_all(bytes).and_then(|()| file.sync_all()))
        .and_then(|()| rename(&temp, path))
        .and_then(|()| fs::File::open(dir).and_then(|dir| dir.sync_all()))
}

/// `path` with `suffix` appended to its file name, next to it.
fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

#[cfg(test)]
mod tests;
