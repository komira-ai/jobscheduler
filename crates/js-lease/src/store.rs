//! The persisted file: one TOML document, replaced whole on every change.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{OpenError, SaveError};
use crate::state::LeaseState;

/// The version this build writes. Readers ignore the number and the fields they do
/// not know, so a later version may add fields and an older build still loads it.
pub(crate) const FORMAT_VERSION: u32 = 1;

/// The whole file.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Document {
    pub version: u32,
    pub term: u64,
    pub next_sequence: u64,
    #[serde(default)]
    pub lease: Vec<StoredLease>,
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
    let temp = temp_path(path);
    let dir = path.parent().filter(|dir| !dir.as_os_str().is_empty());
    let dir = dir.unwrap_or(Path::new("."));
    fs::File::create(&temp)
        .and_then(|mut file| file.write_all(bytes).and_then(|()| file.sync_all()))
        .and_then(|()| fs::rename(&temp, path))
        .and_then(|()| fs::File::open(dir).and_then(|dir| dir.sync_all()))
}

fn temp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    path.with_file_name(name)
}
