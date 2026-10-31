//! The failures of reading and analysing runs.

use std::io;
use std::path::PathBuf;

/// What can go wrong between the input file and the report.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A line of the input is not a run record.
    #[error("line {line}: {source}")]
    Line {
        /// The 1-based line number.
        line: usize,
        /// What the JSON parser said.
        source: serde_json::Error,
    },
    /// Two records share setting, arm and run index.
    #[error("setting {setting:?}, arm {arm:?}: run_index {run_index} appears twice")]
    DuplicateRun {
        /// The setting of both records.
        setting: String,
        /// The arm of both records.
        arm: String,
        /// The repeated run index.
        run_index: u32,
    },
    /// A named arm has no run in the input.
    #[error("no run of arm {0:?}")]
    UnknownArm(String),
    /// The input file cannot be read.
    #[error("cannot read {}: {source}", path.display())]
    Read {
        /// The file.
        path: PathBuf,
        /// The operating system's reason.
        source: io::Error,
    },
    /// Writing the output failed.
    #[error("cannot write the output: {0}")]
    Io(#[from] io::Error),
    /// Writing the JSON report failed.
    #[error("cannot write the JSON report: {0}")]
    Json(#[from] serde_json::Error),
}
