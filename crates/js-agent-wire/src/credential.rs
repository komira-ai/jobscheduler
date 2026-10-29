//! The host credential: the secret a server hands an agent in the [`crate::Welcome`] of
//! the host's first admitted hello, in place of the one-time join token.
//!
//! It is an opaque token of the same alphabet as the session secret. The server mints it
//! (256 random bits, 64 lower-case hex digits) and keeps its digest only. The type has no
//! `Display` and its `Debug` prints `***`, so a log line, an error or a panic message
//! cannot carry the text; [`HostCredential::expose`] is the one way to read it, for the
//! `Authorization` header and the agent's credential file.

use std::fmt;

use serde::{Deserialize, Serialize, Serializer};

use crate::check::{self, At};
use crate::error::ValidationError;
use crate::refusal;

/// The secret that stands in for a join token once a host is registered.
///
/// Decoding refuses text outside `[A-Za-z0-9._-]`, an empty text and one longer than
/// [`crate::limits::MAX_SESSION_LEN`], with an error that names the field and the rule only.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub struct HostCredential(String);

impl HostCredential {
    /// The credential whose text is `text`.
    ///
    /// # Errors
    ///
    /// The text is empty, too long or holds a character outside `[A-Za-z0-9._-]`.
    pub fn parse(text: &str) -> Result<Self, ValidationError> {
        let at = At::new("host_credential", refusal::INVALID_WELCOME);
        check::session(at, text).map(|()| Self(text.to_owned()))
    }

    /// The credential made of 256 random bits, as 64 lower-case hex digits.
    pub fn from_bytes(bytes: &[u8; 32]) -> Self {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut text = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            text.push(char::from(DIGITS[usize::from(byte >> 4)]));
            text.push(char::from(DIGITS[usize::from(byte & 15)]));
        }
        Self(text)
    }

    /// The text, for the header that carries it and the file that keeps it. Nothing else
    /// should read it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for HostCredential {
    type Error = ValidationError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::parse(&text)
    }
}

impl Serialize for HostCredential {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl fmt::Debug for HostCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("HostCredential(***)")
    }
}
