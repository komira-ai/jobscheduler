//! The name a learned profile is kept under.

use std::fmt;

use thiserror::Error;

/// The longest a [`ProfileKey`] may be, in bytes.
pub const MAX_KEY_LEN: usize = 128;

/// Why a string is not a [`ProfileKey`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum BadKey {
    /// The string is empty or longer than [`MAX_KEY_LEN`] bytes.
    #[error("a profile key is 1 to {MAX_KEY_LEN} bytes")]
    Length,
    /// The first byte is not `0-9` or `a-z`.
    #[error("a profile key starts with 0-9 or a-z")]
    First,
    /// The string holds a byte outside `0-9`, `a-z`, `.`, `_` and `-`.
    #[error("a profile key holds only 0-9, a-z, '.', '_' and '-'")]
    Byte,
}

/// The name of an action key's profile: 1 to 128 bytes of `0-9`, `a-z`, `.`, `_`, `-`.
///
/// The grammar is strict so that a key is one URL segment and one state-store key
/// component, and so that nothing a client sends can reach a store key or a log line
/// unchecked. The mapping from a scheduler's `WorkKey` to this name belongs to the
/// admission side ([`crate::ProfileCurves::new`] takes it); this crate stores and serves
/// the string only.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProfileKey(String);

impl ProfileKey {
    /// The key named `text`.
    ///
    /// # Errors
    /// [`BadKey`] when `text` is not 1 to 128 bytes of the allowed set.
    pub fn parse(text: &str) -> Result<Self, BadKey> {
        if text.is_empty() || text.len() > MAX_KEY_LEN {
            return Err(BadKey::Length);
        }
        let allowed = |b: u8| matches!(b, b'0'..=b'9' | b'a'..=b'z' | b'.' | b'_' | b'-');
        if !text.bytes().all(allowed) {
            return Err(BadKey::Byte);
        }
        Ok(Self(text.to_owned()))
    }

    /// The name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProfileKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: a grammar that lets a path separator, dot-dot trick, uppercase or
    /// percent escape through, or one that refuses the legal edge lengths.
    #[test]
    fn the_grammar_admits_only_lowercase_names_of_1_to_128_bytes() {
        for ok in ["a", "0", "x.y_z-9", "a..", &"a".repeat(128)] {
            assert_eq!(ProfileKey::parse(ok).unwrap().as_str(), ok);
        }
        assert_eq!(ProfileKey::parse(""), Err(BadKey::Length));
        assert_eq!(ProfileKey::parse(&"a".repeat(129)), Err(BadKey::Length));
        for bad in ["A", "a/b", "a b", "%2e", "é", "a\n", "a:b", "a*"] {
            assert_eq!(ProfileKey::parse(bad), Err(BadKey::Byte), "{bad:?}");
        }
    }

    /// Asserts that a name starts with a digit or a lower-case letter: `.`, `_` and `-`
    /// are legal inside a name and refused first, with the error that says so. It catches
    /// a name such as `..` or `-rf`, which no caller's key needs and a path or a flag
    /// would misread.
    #[test]
    fn a_name_starts_with_a_digit_or_a_lowercase_letter() {
        for bad in [".", "..", ".a", "_a", "-a", "-"] {
            assert_eq!(ProfileKey::parse(bad), Err(BadKey::First), "{bad:?}");
        }
        assert_eq!(ProfileKey::parse("-A"), Err(BadKey::First));
        assert_eq!(ProfileKey::parse("aA"), Err(BadKey::Byte));
        assert!(BadKey::First.to_string().contains("starts"));
    }

    #[test]
    fn a_key_prints_as_its_name_and_orders_by_it() {
        let a = ProfileKey::parse("a").unwrap();
        let b = ProfileKey::parse("b").unwrap();
        assert_eq!(a.to_string(), "a");
        assert!(a < b);
        assert_eq!(
            BadKey::Length.to_string(),
            "a profile key is 1 to 128 bytes"
        );
        assert!(BadKey::Byte.to_string().contains("only"));
    }
}
