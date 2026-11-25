//! The words a job's completion callback is told in, shared by the store that keeps its
//! delivery and the crate that sends it (`docs/design/job-retry-and-completion-callback.md`,
//! sections 5.2 to 5.5): the key a call is signed with ([`SigningKey`]) and what one try's
//! answer meant ([`CallbackAnswer`], [`FailureClass`]).
//!
//! The key is secret, so nothing here shows it: its `Debug` prints neither the key nor its
//! length, and its refusal names only the length the key had.

use std::fmt;

/// The fewest bytes a [`SigningKey`] holds.
pub const MIN_SIGNING_KEY_LEN: usize = 32;

/// The most bytes a [`SigningKey`] holds.
pub const MAX_SIGNING_KEY_LEN: usize = 64;

/// The key a job's callbacks are signed with: 32 to 64 bytes the submitter chose. Its `Debug`
/// prints nothing of it.
#[derive(Clone, PartialEq, Eq)]
pub struct SigningKey(Vec<u8>);

/// Bytes that are not a [`SigningKey`]: too few or too many. It names their length, never the
/// bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error(
    "a callback signing key is {MIN_SIGNING_KEY_LEN} to {MAX_SIGNING_KEY_LEN} bytes, not {len}"
)]
pub struct SigningKeyError {
    /// The length of the bytes refused.
    pub len: usize,
}

impl SigningKey {
    /// The key `bytes`.
    ///
    /// # Errors
    /// [`SigningKeyError`] when `bytes` holds fewer than [`MIN_SIGNING_KEY_LEN`] or more than
    /// [`MAX_SIGNING_KEY_LEN`] bytes.
    pub fn new(bytes: Vec<u8>) -> Result<SigningKey, SigningKeyError> {
        if (MIN_SIGNING_KEY_LEN..=MAX_SIGNING_KEY_LEN).contains(&bytes.len()) {
            Ok(SigningKey(bytes))
        } else {
            Err(SigningKeyError { len: bytes.len() })
        }
    }

    /// The key's bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for SigningKey {
    /// `SigningKey(..)`, whatever the key: not even its length is shown.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SigningKey(..)")
    }
}

/// What one try of a callback came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallbackAnswer {
    /// The receiver answered 2xx: the callback is delivered.
    Delivered,
    /// The receiver answered 410: it does not want the callback, and nothing more is sent.
    Abandoned,
    /// Any other answer, or none: the call is tried again after a wait.
    Failed(FailureClass),
}

/// The class of a failed try, the only part of it its submitter sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    /// The server's address rule refused the URL's address, so nothing was sent.
    RefusedAddress,
    /// Anything else: another status, a timeout, a refused connection or a redirect.
    Failed,
}

impl FailureClass {
    /// The word the class is shown as: `refused-address` or `failed`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RefusedAddress => "refused-address",
            Self::Failed => "failed",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts that a key of 32 and of 64 bytes is taken byte for byte, that one of 0, 31 and
    /// 65 bytes is refused naming its length, and the refusal's whole text. Catches a bound
    /// off by one at either end, and a refusal that would repeat the key.
    #[test]
    fn a_key_is_32_to_64_bytes() {
        for len in [MIN_SIGNING_KEY_LEN, MAX_SIGNING_KEY_LEN] {
            let bytes: Vec<u8> = (0..len).map(|byte| byte as u8).collect();
            let key = SigningKey::new(bytes.clone()).expect("a key");
            assert_eq!(key.as_bytes(), bytes.as_slice());
        }
        for len in [0, MIN_SIGNING_KEY_LEN - 1, MAX_SIGNING_KEY_LEN + 1] {
            assert_eq!(
                SigningKey::new(vec![b's'; len]),
                Err(SigningKeyError { len })
            );
        }
        assert_eq!(
            SigningKeyError { len: 65 }.to_string(),
            "a callback signing key is 32 to 64 bytes, not 65"
        );
    }

    /// Asserts the whole `Debug` text of two keys of different lengths, alone and inside an
    /// `Option`: `SigningKey(..)`. Catches a derived `Debug` coming back, which prints the
    /// key, and one that shows its length.
    #[test]
    fn a_keys_debug_shows_nothing_of_it() {
        let short = SigningKey::new(b"0123456789abcdef0123456789abcdef".to_vec()).expect("a key");
        let long = SigningKey::new(vec![b'k'; MAX_SIGNING_KEY_LEN]).expect("a key");
        assert_eq!(format!("{short:?}"), "SigningKey(..)");
        assert_eq!(format!("{long:?}"), "SigningKey(..)");
        assert_eq!(format!("{:?}", Some(&short)), "Some(SigningKey(..))");
        assert_eq!(format!("{short:#?}"), "SigningKey(..)");
    }

    /// Asserts the word of each failure class, as `CallbackStatus.last_failure` shows it, and
    /// that a failed answer keeps its class. Catches a class shown in another spelling than
    /// the job API documents.
    #[test]
    fn each_failure_class_has_its_word() {
        assert_eq!(FailureClass::RefusedAddress.as_str(), "refused-address");
        assert_eq!(FailureClass::Failed.as_str(), "failed");
        assert_ne!(
            CallbackAnswer::Failed(FailureClass::RefusedAddress),
            CallbackAnswer::Failed(FailureClass::Failed)
        );
        assert_ne!(CallbackAnswer::Delivered, CallbackAnswer::Abandoned);
    }
}
