//! The field checks the `validate` methods share.
//!
//! Every check names the field and the refusal code of the message it runs for, and
//! returns an error built from those two and the rule alone: no check copies a byte of
//! the value into its error.

use std::collections::{BTreeMap, BTreeSet};

use js_types::{LabelError, Labels};

use crate::error::{Rule, ValidationError};
use crate::limits;
use crate::refusal;

/// The field and refusal code a check reports under.
#[derive(Debug, Clone, Copy)]
pub(crate) struct At {
    pub(crate) field: &'static str,
    pub(crate) code: &'static str,
}

impl At {
    pub(crate) fn new(field: &'static str, code: &'static str) -> Self {
        Self { field, code }
    }

    /// The same code under another field.
    pub(crate) fn field(self, field: &'static str) -> Self {
        Self { field, ..self }
    }

    pub(crate) fn fail(self, rule: Rule) -> ValidationError {
        ValidationError::new(self.field, rule, self.code)
    }
}

/// `[A-Za-z0-9._-]`.
pub(crate) fn ident_byte(byte: u8) -> bool {
    matches!(byte, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-')
}

/// An OCI reference: ident plus `:`, `/`, `@` and `+`.
pub(crate) fn image_byte(byte: u8) -> bool {
    matches!(byte, b':' | b'/' | b'@' | b'+') || ident_byte(byte)
}

/// A label key: lower case, so the reserved prefix cannot be spelled around.
pub(crate) fn label_key_byte(byte: u8) -> bool {
    matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-' | b'/')
}

/// A non-empty token of at most `max` bytes, every byte in `allowed`, the first an ASCII
/// letter or digit.
pub(crate) fn token(
    at: At,
    value: &str,
    max: usize,
    allowed: fn(u8) -> bool,
) -> Result<(), ValidationError> {
    if value.is_empty() {
        return Err(at.fail(Rule::Empty));
    }
    if value.len() > max {
        return Err(at.fail(Rule::TooLong));
    }
    let leads = value.as_bytes()[0].is_ascii_alphanumeric();
    let inside = value.bytes().all(allowed);
    if ![leads, inside].into_iter().all(|ok| ok) {
        return Err(at.fail(Rule::BadChars));
    }
    Ok(())
}

/// An id: [`token`] over `[A-Za-z0-9._-]`, at most [`limits::MAX_ID_LEN`].
pub(crate) fn ident(at: At, value: &str) -> Result<(), ValidationError> {
    token(at, value, limits::MAX_ID_LEN, ident_byte)
}

/// A present id; an absent one passes.
pub(crate) fn opt_ident(at: At, value: Option<&String>) -> Result<(), ValidationError> {
    value.map_or(Ok(()), |text| ident(at, text))
}

/// Non-empty text of at most `max` bytes with no control or
/// format character.
pub(crate) fn text(at: At, value: &str, max: usize) -> Result<(), ValidationError> {
    if value.is_empty() {
        return Err(at.fail(Rule::Empty));
    }
    if value.len() > max {
        return Err(at.fail(Rule::TooLong));
    }
    if value.chars().any(|c| c.is_control() || is_format(c)) {
        return Err(at.fail(Rule::BadChars));
    }
    Ok(())
}

/// Unicode format and separator characters (general category Cf, plus U+2028 and
/// U+2029): none draws a glyph, but each can reorder or hide the text around it.
const FORMAT_RANGES: [(u32, u32); 13] = [
    (0x00AD, 0x00AD),
    (0x0600, 0x0605),
    (0x061C, 0x061C),
    (0x06DD, 0x06DD),
    (0x070F, 0x070F),
    (0x180E, 0x180E),
    (0x200B, 0x200F),
    (0x2028, 0x202E),
    (0x2060, 0x206F),
    (0xFEFF, 0xFEFF),
    (0xFFF9, 0xFFFB),
    (0xE0001, 0xE0001),
    (0xE0020, 0xE007F),
];

fn is_format(c: char) -> bool {
    let code = u32::from(c);
    FORMAT_RANGES
        .iter()
        .any(|&(low, high)| (low..=high).contains(&code))
}

/// Present text; absent text passes.
pub(crate) fn opt_text(at: At, value: Option<&String>, max: usize) -> Result<(), ValidationError> {
    value.map_or(Ok(()), |text_value| text(at, text_value, max))
}

/// At most `max` entries.
pub(crate) fn count(at: At, len: usize, max: usize) -> Result<(), ValidationError> {
    if len > max {
        return Err(at.fail(Rule::TooMany));
    }
    Ok(())
}

/// A number from `min` to `max`, both included.
pub(crate) fn range(at: At, value: u64, min: u64, max: u64) -> Result<(), ValidationError> {
    if !(min..=max).contains(&value) {
        return Err(at.fail(Rule::OutOfRange));
    }
    Ok(())
}

/// No entry repeats.
pub(crate) fn distinct<T: Ord>(
    at: At,
    items: impl IntoIterator<Item = T>,
) -> Result<(), ValidationError> {
    let mut seen = BTreeSet::new();
    for item in items {
        if !seen.insert(item) {
            return Err(at.fail(Rule::Duplicate));
        }
    }
    Ok(())
}

/// A set of labels: at most [`limits::MAX_LABELS`], keys of [`label_key_byte`] bounded in
/// length, then `js-types`' own rules for the reserved prefix and the values. A reserved
/// key reports [`refusal::RESERVED_LABEL`]; any other fault reports the code of `at`.
pub(crate) fn labels(at: At, map: &BTreeMap<String, String>) -> Result<(), ValidationError> {
    count(at, map.len(), limits::MAX_LABELS)?;
    for key in map.keys() {
        token(at, key, limits::MAX_LABEL_KEY_LEN, label_key_byte)?;
    }
    Labels::from_caller(map).map_err(|fault| match fault {
        LabelError::Reserved(_) => {
            ValidationError::new(at.field, Rule::Reserved, refusal::RESERVED_LABEL)
        }
        _ => at.fail(Rule::BadChars),
    })?;
    Ok(())
}
