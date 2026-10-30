//! The list files: manifest digests, one a line.

use std::collections::BTreeSet;

/// A list file that cannot be read as a list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("line {line} is not a sha256 digest")]
pub struct ListError {
    /// The line, counted from 1. Its text is not repeated: it is not trusted.
    pub line: usize,
}

/// The digests of a list file: `sha256:` and 64 lower case hex digits on a line, blank lines
/// and lines that start with `#` ignored, spaces around a digest ignored.
///
/// # Errors
///
/// The first line that is none of these.
pub fn parse_digests(text: &str) -> Result<BTreeSet<String>, ListError> {
    let mut found = BTreeSet::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if !is_digest(line) {
            return Err(ListError { line: index + 1 });
        }
        found.insert(line.to_owned());
    }
    Ok(found)
}

/// `sha256:` and 64 lower case hex digits.
pub(crate) fn is_digest(text: &str) -> bool {
    let hex = text.strip_prefix("sha256:").unwrap_or("");
    hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}
