//! A job's outputs in the store (`docs/design/job-outputs.md`): the files a job declared, put
//! after it ends at `P/outputs/<path>` under the job's prefix `P`, and the manifest written last,
//! `P/outputs.json`. The agent that puts them and the server that reads them both name the keys,
//! check the paths and read the manifest here, so they agree.
//!
//! A path ([`OutputPath`]) is 1 to [`MAX_PATH_LEN`] bytes of segments joined by `/`, each 1 to
//! [`MAX_SEGMENT_LEN`] bytes of `[A-Za-z0-9._-]` and neither `.` nor `..`: a key segment under
//! every kind of credential, a presigned-prefix grant's included. A declaration
//! ([`check_declared`]) is at most [`MAX_DECLARED`] such paths, none twice and none inside
//! another.

use std::fmt;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::credential::{Credential, Endpoint};
use crate::layout::{JobPrefix, segment_ok};
use crate::sigv4::{Scope, Unsigned, canonical_query, presign};

/// The most outputs a job declares.
pub const MAX_DECLARED: usize = 64;

/// The longest output path, in bytes.
pub const MAX_PATH_LEN: usize = 512;

/// The longest segment of an output path, in bytes.
pub const MAX_SEGMENT_LEN: usize = 255;

/// The longest manifest, in bytes; a reader refuses a longer one.
pub const MAX_MANIFEST_BYTES: usize = 1024 * 1024;

/// The version of the manifest this crate writes and reads.
pub const MANIFEST_VERSION: u32 = 1;

/// The directory of a job's output files under its prefix.
const OUTPUTS_DIR: &str = "outputs/";

/// The manifest's name under a job's prefix.
const MANIFEST_NAME: &str = "outputs.json";

/// A path of an output: below the job's working directory, and below `P/outputs/` in the store.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OutputPath(String);

/// Why a path or a declaration is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum OutputPathError {
    /// The path is empty or longer than [`MAX_PATH_LEN`].
    #[error("is empty or longer than {MAX_PATH_LEN} bytes")]
    Length,
    /// A segment is empty, `.` or `..`, longer than [`MAX_SEGMENT_LEN`], or holds a byte outside
    /// `[A-Za-z0-9._-]`.
    #[error(
        "has a segment that is empty, . or .., longer than {MAX_SEGMENT_LEN} bytes, or holds a byte outside [A-Za-z0-9._-]"
    )]
    Segment,
    /// The path was declared before.
    #[error("is declared twice")]
    Twice,
    /// The path is inside another declared path, or another is inside it.
    #[error("is inside another output, or another is inside it")]
    Nested,
    /// The declaration holds more than [`MAX_DECLARED`] paths.
    #[error("is past the {MAX_DECLARED} outputs a job may declare")]
    TooMany,
}

impl OutputPath {
    /// The path `text`.
    ///
    /// # Errors
    ///
    /// [`OutputPathError::Length`] or [`OutputPathError::Segment`] when `text` breaks the rules
    /// of the module docs.
    pub fn parse(text: &str) -> Result<Self, OutputPathError> {
        if text.is_empty() || text.len() > MAX_PATH_LEN {
            return Err(OutputPathError::Length);
        }
        let segment = |part: &str| part.len() <= MAX_SEGMENT_LEN && segment_ok(part);
        match text.split('/').all(segment) {
            true => Ok(Self(text.to_owned())),
            false => Err(OutputPathError::Segment),
        }
    }

    /// The path below `self` named `name`, a segment.
    ///
    /// # Errors
    ///
    /// What [`OutputPath::parse`] says of the joined path.
    pub fn join(&self, name: &str) -> Result<Self, OutputPathError> {
        Self::parse(&format!("{}/{name}", self.0))
    }

    /// The text of the path.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether `self` is `other` or below it.
    #[must_use]
    pub fn within(&self, other: &OutputPath) -> bool {
        self.0 == other.0
            || self
                .0
                .strip_prefix(&other.0)
                .is_some_and(|rest| rest.starts_with('/'))
    }

    /// The segments of the path, in order.
    pub fn segments(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }
}

impl fmt::Display for OutputPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The paths of a declaration, checked: each a path, none twice, none inside another, at most
/// [`MAX_DECLARED`].
///
/// # Errors
///
/// The index of the first path at fault and why.
pub fn check_declared(paths: &[String]) -> Result<Vec<OutputPath>, (usize, OutputPathError)> {
    let mut checked: Vec<OutputPath> = Vec::with_capacity(paths.len());
    for (at, text) in paths.iter().enumerate() {
        if at == MAX_DECLARED {
            return Err((at, OutputPathError::TooMany));
        }
        let path = OutputPath::parse(text).map_err(|why| (at, why))?;
        if checked.contains(&path) {
            return Err((at, OutputPathError::Twice));
        }
        let nested = checked
            .iter()
            .any(|other| path.within(other) || other.within(&path));
        if nested {
            return Err((at, OutputPathError::Nested));
        }
        checked.push(path);
    }
    Ok(checked)
}

impl JobPrefix {
    /// The key of the output file `path`: `P` + `outputs/` + the path.
    #[must_use]
    pub fn output_key(&self, path: &OutputPath) -> String {
        format!("{}{OUTPUTS_DIR}{path}", self.as_str())
    }

    /// The key of the outputs manifest: `P` + `outputs.json`.
    #[must_use]
    pub fn outputs_manifest_key(&self) -> String {
        format!("{}{MANIFEST_NAME}", self.as_str())
    }
}

/// One output file the manifest lists: put whole at its key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputFile {
    /// Its path, a declared path or one below a declared directory.
    pub path: String,
    /// Its length in bytes.
    pub bytes: u64,
    /// The SHA-256 of its bytes, 64 lower-case hex digits.
    pub sha256: String,
}

/// One path the agent could not put, and why: a word such as `not-a-file`, `too-large`,
/// `over-limit`, `name`, `read` or `store`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputFailure {
    /// The path, as declared or below a declared directory (a name that is not a segment is
    /// shown with each byte outside `[A-Za-z0-9._-]` as `?`).
    pub path: String,
    /// Why.
    pub reason: String,
}

/// The manifest: what the agent put, what was missing, and what failed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// [`MANIFEST_VERSION`].
    pub version: u32,
    /// The files put, in the order they were.
    #[serde(default)]
    pub files: Vec<OutputFile>,
    /// The declared paths the job did not make.
    #[serde(default)]
    pub missing: Vec<String>,
    /// The paths not put.
    #[serde(default)]
    pub failed: Vec<OutputFailure>,
}

/// Why a manifest cannot be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ManifestError {
    /// It is longer than [`MAX_MANIFEST_BYTES`].
    #[error("the manifest is longer than {MAX_MANIFEST_BYTES} bytes")]
    TooLong,
    /// It is not the JSON of a manifest.
    #[error("the manifest is not a manifest: {0}")]
    Malformed(String),
    /// Its version is not [`MANIFEST_VERSION`].
    #[error("the manifest is of version {0}, not {MANIFEST_VERSION}")]
    Version(u32),
    /// A file's path is not an output path within a declared one, or its digest is not 64
    /// lower-case hex digits.
    #[error("the manifest lists a file that is not an output of the job: {0:?}")]
    Stray(String),
}

impl Manifest {
    /// The manifest's bytes, JSON.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }

    /// The manifest of `bytes`, a job's whose declared outputs are `declared`; fields it does
    /// not know are ignored.
    ///
    /// # Errors
    ///
    /// [`ManifestError`] says why it cannot be read.
    pub fn decode(bytes: &[u8], declared: &[OutputPath]) -> Result<Self, ManifestError> {
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err(ManifestError::TooLong);
        }
        let manifest: Self = serde_json::from_slice(bytes)
            .map_err(|why| ManifestError::Malformed(why.to_string()))?;
        if manifest.version != MANIFEST_VERSION {
            return Err(ManifestError::Version(manifest.version));
        }
        let stray = manifest.files.iter().find(|file| {
            let path = OutputPath::parse(&file.path);
            let inside = path.is_ok_and(|path| declared.iter().any(|outer| path.within(outer)));
            !inside || !is_sha256(&file.sha256)
        });
        match stray {
            Some(file) => Err(ManifestError::Stray(file.path.clone())),
            None => Ok(manifest),
        }
    }
}

/// Whether `text` is 64 lower-case hex digits.
fn is_sha256(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// A presigned GET of `key` at `endpoint`, signed with `credential` at `now`, which the store
/// answers for `expires_secs` seconds (clamped to SigV4's 1 second to 7 days): a URL that reads
/// that one object and nothing else.
#[must_use]
pub fn presigned_get(
    endpoint: &Endpoint,
    credential: &Credential,
    key: &str,
    now: SystemTime,
    expires_secs: u64,
) -> String {
    let host = endpoint.host();
    let path = endpoint.object_path(key);
    let unsigned = Unsigned {
        method: "GET",
        host: &host,
        path: &path,
        query: &[],
        headers: &[],
        sign_also: &[],
        body: &[],
    };
    let scope = Scope {
        region: endpoint.region(),
        service: "s3",
    };
    let query = presign(&unsigned, credential, &scope, now, expires_secs);
    format!("{}?{}", endpoint.url(&path), canonical_query(&query))
}
