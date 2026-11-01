//! The join token as the host receives it: 64 lower-case hex digits on standard input.

use std::fmt;
use std::io::{self, Read};

use sha2::{Digest, Sha256};

/// The digits of a token the server mints.
const DIGITS: usize = 64;

/// How much of standard input is read: a token, its line break and one byte more, so that a
/// stream that goes on is told from a token without reading all of it.
const READ_LIMIT: u64 = DIGITS as u64 + 2;

/// The characters a token is made of.
const HEX: &[u8] = b"0123456789abcdef";

/// A join token. It has no text form but its sha256: `Debug` hides it and no error holds it.
pub struct Token {
    digits: String,
}

/// Why standard input is not a join token.
#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    /// Standard input could not be read.
    #[error("cannot read standard input: {0}")]
    Read(io::Error),
    /// Standard input is not a token. The rule that failed is named, never the input.
    #[error("the token on standard input {0}")]
    Invalid(&'static str),
}

impl fmt::Debug for Token {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str("Token(<redacted>)")
    }
}

impl Token {
    /// The token in `bytes`: 64 lower-case hex digits, and at most one line break after
    /// them, as `js lease join-token mint` and the server's own answer give it.
    ///
    /// # Errors
    ///
    /// [`TokenError::Invalid`] saying which rule failed.
    pub fn parse(bytes: &[u8]) -> Result<Self, TokenError> {
        let body = bytes.strip_suffix(b"\n").unwrap_or(bytes);
        let problems = [
            (body.is_empty(), "is empty"),
            (body.contains(&b'\n'), "has more than one line"),
            (body.len() != DIGITS, "is not 64 characters long"),
            (
                !body.iter().all(|byte| HEX.contains(byte)),
                "is not lower-case hex digits",
            ),
        ];
        match problems.iter().find(|(bad, _)| *bad) {
            Some((_, why)) => Err(TokenError::Invalid(why)),
            None => Ok(Self {
                digits: String::from_utf8_lossy(body).into_owned(),
            }),
        }
    }

    /// The token at the start of `from`. No more than a token, its line break and one byte
    /// more is read.
    ///
    /// # Errors
    ///
    /// [`TokenError::Read`] when `from` fails, or [`TokenError::Invalid`] as for
    /// [`Token::parse`].
    pub fn read(from: &mut dyn Read) -> Result<Self, TokenError> {
        let mut bytes = Vec::new();
        Read::take(from, READ_LIMIT)
            .read_to_end(&mut bytes)
            .map_err(TokenError::Read)?;
        Self::parse(&bytes)
    }

    /// The sha256 of the token's digits, as 64 lower-case hex digits: the one thing about a
    /// token the verb prints.
    #[must_use]
    pub fn sha256(&self) -> String {
        hex::encode(Sha256::digest(self.digits.as_bytes()))
    }

    /// What the token file holds: the digits and a line break.
    pub(super) fn line(&self) -> String {
        format!("{}\n", self.digits)
    }
}
