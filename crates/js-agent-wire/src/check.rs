//! The field checks the `validate` methods share.
//!
//! Every check names the field and the refusal code of the message it runs for, and
//! returns an error built from those two and the rule alone: no check copies a byte of
//! the value into its error.

use std::collections::{BTreeMap, BTreeSet};

use js_types::{Labels, RESERVED_PREFIX};

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
/// `[a-z0-9]`.
pub(crate) fn lower_alnum(byte: u8) -> bool {
    matches!(byte, b'a'..=b'z' | b'0'..=b'9')
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

/// A session secret: [`token`] over `[A-Za-z0-9._-]`, at most [`limits::MAX_SESSION_LEN`].
pub(crate) fn session(at: At, value: &str) -> Result<(), ValidationError> {
    token(at, value, limits::MAX_SESSION_LEN, ident_byte)
}

/// A present id; an absent one passes.
pub(crate) fn opt_ident(at: At, value: Option<&String>) -> Result<(), ValidationError> {
    value.map_or(Ok(()), |text| ident(at, text))
}

/// Non-empty text of at most `max` bytes with no control or format character.
pub(crate) fn text(at: At, value: &str, max: usize) -> Result<(), ValidationError> {
    if value.is_empty() {
        return Err(at.fail(Rule::Empty));
    }
    if value.len() > max {
        return Err(at.fail(Rule::TooLong));
    }
    if value.chars().any(hidden) {
        return Err(at.fail(Rule::BadChars));
    }
    Ok(())
}

/// A character that must not appear in text a person or a log may read: a control
/// character, a format character or a line or paragraph separator.
pub(crate) fn hidden(c: char) -> bool {
    c.is_control() || is_format(c)
}

/// The Unicode format characters (general category Cf) of Unicode 16.0, as listed by
/// UCD 16.0.0 (the data `regex-syntax` 0.8.11 and `unicode-general-category` 1.1.0 are
/// generated from), and U+2028 and U+2029 (categories Zl and Zp). None draws a glyph,
/// but each can reorder or hide the text around it. The tests name a code point at both
/// ends of every range and one just outside each.
const FORMAT_RANGES: [(u32, u32); 21] = [
    (0x00AD, 0x00AD),
    (0x0600, 0x0605),
    (0x061C, 0x061C),
    (0x06DD, 0x06DD),
    (0x070F, 0x070F),
    (0x0890, 0x0891),
    (0x08E2, 0x08E2),
    (0x180E, 0x180E),
    (0x200B, 0x200F),
    (0x2028, 0x202E),
    (0x2060, 0x2064),
    (0x2066, 0x206F),
    (0xFEFF, 0xFEFF),
    (0xFFF9, 0xFFFB),
    (0x110BD, 0x110BD),
    (0x110CD, 0x110CD),
    (0x13430, 0x1343F),
    (0x1BCA0, 0x1BCA3),
    (0x1D173, 0x1D17A),
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

/// The arguments of a job: [`limits::MAX_ARGS`] entries of [`limits::MAX_ARG_LEN`] bytes,
/// [`limits::MAX_ARGS_TOTAL_BYTES`] in all, no NUL.
pub(crate) fn args(at: At, args: &[String]) -> Result<(), ValidationError> {
    count(at, args.len(), limits::MAX_ARGS)?;
    let mut total = 0usize;
    for arg in args {
        if arg.len() > limits::MAX_ARG_LEN {
            return Err(at.fail(Rule::TooLong));
        }
        if arg.contains('\0') {
            return Err(at.fail(Rule::BadChars));
        }
        total += arg.len();
    }
    if total > limits::MAX_ARGS_TOTAL_BYTES {
        return Err(at.fail(Rule::TooLong));
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

/// A label key: `name` or `prefix/name`, with at most one `/`. The prefix is a DNS name
/// (dot-separated labels of lower-case letters, digits and `-`, each starting and ending
/// with a letter or digit) and the name starts with a lower-case letter or digit, then
/// holds `[a-z0-9._-]`. A key under the reserved prefix (or the prefix alone) reports
/// [`refusal::RESERVED_LABEL`]; any other fault reports the code of `at`.
fn label_key(at: At, key: &str) -> Result<(), ValidationError> {
    let reserved = key.starts_with(RESERVED_PREFIX) || key == RESERVED_PREFIX.trim_end_matches('/');
    if reserved {
        return Err(ValidationError::new(
            at.field,
            Rule::Reserved,
            refusal::RESERVED_LABEL,
        ));
    }
    if key.len() > limits::MAX_LABEL_KEY_LEN {
        return Err(at.fail(Rule::TooLong));
    }
    let (prefix, name) = key
        .split_once('/')
        .map_or((None, key), |(prefix, name)| (Some(prefix), name));
    token(at, name, limits::MAX_LABEL_NAME_LEN, label_name_byte)?;
    if prefix.is_some_and(|p| {
        p.split('.')
            .any(|part| part.len() > limits::MAX_DNS_LABEL_LEN)
    }) {
        return Err(at.fail(Rule::TooLong));
    }
    if !prefix.is_none_or(is_dns_name) {
        return Err(at.fail(Rule::BadChars));
    }
    Ok(())
}

/// `[a-z0-9._-]`.
fn label_name_byte(byte: u8) -> bool {
    matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-')
}

/// Dot-separated DNS labels: non-empty, `[a-z0-9-]`, a letter or digit at both ends.
fn is_dns_name(name: &str) -> bool {
    name.split('.').all(|part| {
        let bytes = part.as_bytes();
        let edge = |byte: Option<&u8>| byte.is_some_and(|b| lower_alnum(*b));
        let inside = bytes.iter().all(|b| lower_alnum(*b) || *b == b'-');
        [edge(bytes.first()), edge(bytes.last()), inside]
            .into_iter()
            .all(|ok| ok)
    })
}

/// A set of labels: at most [`limits::MAX_LABELS`], each key as [`label_key`] says, then
/// `js-types`' own rules for the values.
pub(crate) fn labels(at: At, map: &BTreeMap<String, String>) -> Result<(), ValidationError> {
    count(at, map.len(), limits::MAX_LABELS)?;
    for key in map.keys() {
        label_key(at, key)?;
    }
    Labels::from_caller(map).map_err(|_| at.fail(Rule::BadChars))?;
    Ok(())
}
