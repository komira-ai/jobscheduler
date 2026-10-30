//! Validated names: a tree name and a diff id.

use serde::{Deserialize, Serialize};

/// A text that is not a valid tree name, diff id or dotted version.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid {what}: {text:?}")]
pub struct FieldParseError {
    /// What was being parsed.
    pub what: &'static str,
    /// The text, as given.
    pub text: String,
}

impl FieldParseError {
    pub(crate) fn new(what: &'static str, text: &str) -> FieldParseError {
        FieldParseError {
            what,
            text: text.to_string(),
        }
    }
}

/// The name of a tree of a bundle, `[a-z][a-z0-9-]{0,31}`. It appears in
/// `${tree:NAME}` placeholders.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct TreeName(String);

impl TreeName {
    /// The longest tree name, in bytes.
    pub const MAX_LEN: usize = 32;

    /// Parses a tree name.
    pub fn parse(text: &str) -> Result<TreeName, FieldParseError> {
        let mut bytes = text.bytes();
        let first_ok = bytes.next().is_some_and(|b| b.is_ascii_lowercase());
        let rest_ok = bytes.all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-'));
        if first_ok && rest_ok && text.len() <= TreeName::MAX_LEN {
            Ok(TreeName(text.to_string()))
        } else {
            Err(FieldParseError::new("tree name", text))
        }
    }

    /// The name as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for TreeName {
    type Error = FieldParseError;

    fn try_from(text: String) -> Result<TreeName, FieldParseError> {
        TreeName::parse(&text)
    }
}

impl From<TreeName> for String {
    fn from(name: TreeName) -> String {
        name.0
    }
}

/// The digest of the uncompressed tar of a layer: `sha256:` and 64 lowercase hex
/// digits. It names the tree's directory in the store.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct DiffId(String);

const DIFF_ID_PREFIX: &str = "sha256:";

impl DiffId {
    /// Parses a diff id.
    pub fn parse(text: &str) -> Result<DiffId, FieldParseError> {
        let hex = text.strip_prefix(DIFF_ID_PREFIX).unwrap_or("");
        let hex_ok = hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        if hex_ok {
            Ok(DiffId(text.to_string()))
        } else {
            Err(FieldParseError::new("diff id", text))
        }
    }

    /// The id of a SHA-256 digest (32 bytes).
    pub(crate) fn from_sha256(digest: impl AsRef<[u8]>) -> DiffId {
        DiffId(format!("{DIFF_ID_PREFIX}{}", hex::encode(digest)))
    }

    /// The id as text, with its `sha256:` prefix.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The 64 hex digits, the name of the tree's directory.
    pub fn hex(&self) -> &str {
        &self.0[DIFF_ID_PREFIX.len()..]
    }
}

impl TryFrom<String> for DiffId {
    type Error = FieldParseError;

    fn try_from(text: String) -> Result<DiffId, FieldParseError> {
        DiffId::parse(&text)
    }
}

impl From<DiffId> for String {
    fn from(id: DiffId) -> String {
        id.0
    }
}
