//! A dotted numeric version, `major.minor[.patch]`.

use serde::{Deserialize, Serialize};

use crate::names::FieldParseError;

/// A macOS or Xcode version, compared numerically (`14.10` is above `14.9`). The text
/// form is `major.minor` or `major.minor.patch`, each part decimal digits without a
/// leading zero, at most nine digits. A missing patch is zero, so `14.0` and `14.0.0` are equal and both are written
/// back as `14.0`; a patch other than zero is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct DottedVersion {
    major: u32,
    minor: u32,
    patch: u32,
}

fn part(text: &str) -> Option<u32> {
    let plain = !text.is_empty() && text.len() <= 9 && text.bytes().all(|b| b.is_ascii_digit());
    let no_zero_lead = text == "0" || !text.starts_with('0');
    if plain && no_zero_lead {
        text.parse().ok()
    } else {
        None
    }
}

impl DottedVersion {
    /// Parses `major.minor[.patch]`.
    pub fn parse(text: &str) -> Result<DottedVersion, FieldParseError> {
        let parts: Vec<Option<u32>> = text.split('.').map(part).collect();
        match parts.as_slice() {
            [Some(major), Some(minor)] => Ok(DottedVersion {
                major: *major,
                minor: *minor,
                patch: 0,
            }),
            [Some(major), Some(minor), Some(patch)] => Ok(DottedVersion {
                major: *major,
                minor: *minor,
                patch: *patch,
            }),
            _ => Err(FieldParseError::new("version", text)),
        }
    }

    /// The major number.
    pub fn major(&self) -> u32 {
        self.major
    }

    /// The minor number.
    pub fn minor(&self) -> u32 {
        self.minor
    }

    /// The patch number, zero when the text had none.
    pub fn patch(&self) -> u32 {
        self.patch
    }
}

impl TryFrom<String> for DottedVersion {
    type Error = FieldParseError;

    fn try_from(text: String) -> Result<DottedVersion, FieldParseError> {
        DottedVersion::parse(&text)
    }
}

impl From<DottedVersion> for String {
    fn from(version: DottedVersion) -> String {
        if version.patch != 0 {
            format!("{}.{}.{}", version.major, version.minor, version.patch)
        } else {
            format!("{}.{}", version.major, version.minor)
        }
    }
}
