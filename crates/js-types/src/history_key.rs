//! The key a job's usage history is kept under, as its client sends it in the job's own field
//! (`JobSpec.history_key`, `docs/design/sizing.md`, section 7.7): the work the job runs, one
//! key per kind of work. For a build action it is the full path of its build target and the
//! kind of the action, as the build tool names them (`komira//src/json:json mojo_compile`).
//! The key names the job's class: the scheduler keys the class's history by it, logs it and
//! keeps it in the job's usage record as it is, never lowered, cut or hashed into another name.
//!
//! A key is 1 to [`MAX_HISTORY_KEY_LEN`] bytes of printable ASCII (`0x20` to `0x7e`): a
//! space, a slash or a colon is kept, and a line break, a tab or any other control byte, or
//! any byte that is not ASCII, is refused, so the key always fits on one line of a log.

/// The longest a [`HistoryKey`] may be, in bytes.
pub const MAX_HISTORY_KEY_LEN: usize = 512;

/// Why a text is not a [`HistoryKey`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HistoryKeyError {
    /// The text is empty or longer than [`MAX_HISTORY_KEY_LEN`] bytes.
    #[error("a history key is 1 to {MAX_HISTORY_KEY_LEN} bytes")]
    Length,
    /// The text holds a byte outside printable ASCII.
    #[error("a history key holds only printable ASCII (0x20 to 0x7e)")]
    Byte,
}

impl HistoryKeyError {
    /// The word a refusal names the fault by: `bad-length` or `not-printable-ascii`.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::Length => "bad-length",
            Self::Byte => "not-printable-ascii",
        }
    }
}

/// A job's history key as its client sent it: 1 to [`MAX_HISTORY_KEY_LEN`] bytes of printable
/// ASCII, kept byte for byte (see the module docs).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HistoryKey(String);

impl HistoryKey {
    /// The key `text`.
    ///
    /// # Errors
    /// [`HistoryKeyError::Length`] for an empty text or one over [`MAX_HISTORY_KEY_LEN`]
    /// bytes, else [`HistoryKeyError::Byte`] for one holding a byte outside `0x20..=0x7e`.
    pub fn parse(text: &str) -> Result<Self, HistoryKeyError> {
        if text.is_empty() || text.len() > MAX_HISTORY_KEY_LEN {
            return Err(HistoryKeyError::Length);
        }
        if !text.bytes().all(|byte| matches!(byte, 0x20..=0x7e)) {
            return Err(HistoryKeyError::Byte);
        }
        Ok(Self(text.to_owned()))
    }

    /// The key, as the client sent it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts that a build target's full path with its action kind is kept byte for byte, at
    /// every length from 1 to the bound, with every printable byte. Catches a key lowered,
    /// cut or rewritten, and a bound or alphabet narrower than the field's.
    #[test]
    fn a_printable_key_of_1_to_512_bytes_is_kept_as_it_is() {
        let printable: String = (0x20u8..=0x7e).map(char::from).collect();
        let long = format!("komira//{}:json mojo_compile build", "src/".repeat(120));
        let longest = "x".repeat(MAX_HISTORY_KEY_LEN);
        for text in [
            "a",
            " ",
            "komira//src/json:json mojo_compile",
            printable.as_str(),
            &long[..200],
            longest.as_str(),
        ] {
            assert_eq!(HistoryKey::parse(text).unwrap().as_str(), text);
        }
    }

    /// Asserts the two refusals and which wins. Catches an empty key taken as a class, a
    /// key one byte over the bound kept, and a control byte (a line break that would split a
    /// log line, a tab, DEL) or a byte above ASCII let through.
    #[test]
    fn an_empty_or_long_key_or_one_with_a_control_or_non_ascii_byte_is_refused() {
        let over = "x".repeat(MAX_HISTORY_KEY_LEN + 1);
        assert_eq!(HistoryKey::parse(""), Err(HistoryKeyError::Length));
        assert_eq!(HistoryKey::parse(&over), Err(HistoryKeyError::Length));
        let over_and_bad = format!("{over}\n");
        assert_eq!(
            HistoryKey::parse(&over_and_bad),
            Err(HistoryKeyError::Length)
        );
        for bad in ["two\nlines", "tab\there", "nul\0", "del\x7f", "us\x1f", "é"] {
            assert_eq!(
                HistoryKey::parse(bad),
                Err(HistoryKeyError::Byte),
                "{bad:?}"
            );
        }
    }

    /// Asserts the words a refusal names each fault by, and the messages. Catches a code
    /// changed under a client that matches on it.
    #[test]
    fn each_fault_has_its_code_and_message() {
        assert_eq!(HistoryKeyError::Length.code(), "bad-length");
        assert_eq!(HistoryKeyError::Byte.code(), "not-printable-ascii");
        assert_eq!(
            HistoryKeyError::Length.to_string(),
            "a history key is 1 to 512 bytes"
        );
        assert_eq!(
            HistoryKeyError::Byte.to_string(),
            "a history key holds only printable ASCII (0x20 to 0x7e)"
        );
    }
}
