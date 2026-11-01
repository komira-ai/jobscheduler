//! The persisted file: one TOML document, replaced whole on every change.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use js_types::Resources;
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
    /// Whether the held host was quiet when it was set aside. Absent is not; a build that
    /// predates the field reads the row without it and keeps it on rewrite.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub assigned: bool,
    /// The end of the held host's tail when it was set aside; only on a row that holds a
    /// host. Absent is unknown; a build that predates the field reads the row without it
    /// and keeps it on rewrite.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tail_end_ms: Option<u64>,
    /// The place on the wait list and when the request was first presented, both or
    /// neither.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_place: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queued_since_ms: Option<u64>,
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
    /// What the lease books when its class lets leases share a host; absent for a lease
    /// that books the whole host and in a file written before the field existed.
    #[serde(
        default,
        skip_serializing_if = "unwritten",
        deserialize_with = "lenient_shares"
    )]
    pub shares: Option<SharesRow>,
    /// Fields of a later version, kept as read.
    #[serde(flatten)]
    pub extra: toml::Table,
}

/// The resources a lease that shares its host books, as written. A field this build does
/// not know is ignored.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct StoredShares {
    pub cpu_millis: u64,
    pub memory_bytes: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub gpus: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_class: Option<String>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub disk_bytes: u64,
}

/// A `shares` value as read: a row, or something that is not one.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub(crate) enum SharesRow {
    /// A table with the counts.
    Good(StoredShares),
    /// Anything else (not a table, a missing or mistyped count). It is never written back.
    Bad,
}

/// Whether a `shares` row is left out of the file: all but a good row.
fn unwritten(row: &Option<SharesRow>) -> bool {
    !matches!(row, Some(SharesRow::Good(_)))
}

/// Reads a `shares` value as a row, or as [`SharesRow::Bad`] when it is not one: a bad row
/// must not fail the whole file, and the lease it belongs to is then put back as booking
/// the whole host (with a warning that names it, see `Record::restore`).
fn lenient_shares<'de, D>(deserializer: D) -> Result<Option<SharesRow>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<toml::Value>::deserialize(deserializer)?;
    Ok(value.map(|value| match value.try_into() {
        Ok(row) => SharesRow::Good(row),
        Err(_) => SharesRow::Bad,
    }))
}

impl StoredShares {
    /// The row that writes `resources`.
    pub fn of(resources: &Resources) -> Self {
        Self {
            cpu_millis: resources.cpu_millis(),
            memory_bytes: resources.memory_bytes(),
            gpus: resources.gpus(),
            gpu_class: resources.gpu_class().map(str::to_owned),
            disk_bytes: resources.disk_bytes(),
        }
    }

    /// The resources the row says, or `None` when they are not a valid vector (GPUs
    /// without a class, a class without GPUs): the lease then reads as booking the whole
    /// host, as a file from before the field does.
    pub fn resources(&self) -> Option<Resources> {
        let counted = Resources::new(
            self.cpu_millis,
            self.memory_bytes,
            self.gpus,
            self.gpu_class.clone(),
        );
        Some(counted.ok()?.with_disk_bytes(self.disk_bytes))
    }
}

/// The exclusive lock of a table's file, released when this is dropped.
///
/// An `flock` belongs to the open file description, not to the descriptor, and a
/// description lives until every copy of the descriptor is closed. A process forked while
/// the lock is held (any spawn of a child by another thread of this process, as in a test
/// binary that restarts a server on one thread and starts programs on others) carries a
/// copy until it execs, so closing the file would leave the table locked for as long as
/// that takes. Dropping unlocks first, which no copy outlives, so the file can be opened
/// again once the drop returns.
#[derive(Debug)]
pub(crate) struct Lock(fs::File);

impl Drop for Lock {
    fn drop(&mut self) {
        // An error leaves the close that follows to release the lock, as before.
        let _ = self.0.unlock();
    }
}

/// Takes the exclusive lock of the table at `path`: a sibling `.lock` file, locked for
/// as long as the returned [`Lock`] lives. A second table over the same path fails here
/// with a `WouldBlock` error instead of issuing the same ids.
pub(crate) fn lock(path: &Path) -> Result<Lock, OpenError> {
    let lock = sibling(path, ".lock");
    let mut options = fs::OpenOptions::new();
    options.create(true).truncate(false).write(true);
    let locked = options.open(&lock).and_then(|file| {
        file.try_lock()
            .map(|()| Lock(file))
            .map_err(io::Error::from)
    });
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
