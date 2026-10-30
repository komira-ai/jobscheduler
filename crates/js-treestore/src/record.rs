//! The record beside a sealed tree: what it is and which extractor made it.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// `trees/<hex>.sealed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Record {
    pub diff_id: String,
    pub blob: String,
    pub extractor_version: u32,
    pub bytes: u64,
}

impl Record {
    /// The record at `path`, when it is there, parses and names `diff_id`.
    pub(crate) fn read(path: &Path, diff_id: &str) -> Option<Record> {
        let bytes = std::fs::read(path).ok()?;
        let record: Record = serde_json::from_slice(&bytes).ok()?;
        (record.diff_id == diff_id).then_some(record)
    }

    pub(crate) fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }
}
