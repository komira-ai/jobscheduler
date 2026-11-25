//! The name of a job's size class as its client sends it in the job's own field
//! (`JobSpec.size_class`, `docs/design/sizing.md`, section 7.7): the work the job runs, such as
//! a build target written as its build tool names it (`root//crates/js-sched:js-sched
//! rust_compile build`). The scheduler keys the class's history by it, logs it and keeps it
//! in the job's usage record as it is: it is never lowered, cut or hashed into another name.
//!
//! A name is 1 to [`MAX_SIZE_CLASS_LEN`] bytes of printable ASCII (`0x20` to `0x7e`): a
//! space, a slash or a colon is kept, and a line break, a tab or any other control byte, or
//! any byte that is not ASCII, is refused, so the name always fits on one line of a log.

/// The longest a [`SizeClassName`] may be, in bytes.
pub const MAX_SIZE_CLASS_LEN: usize = 512;

/// Why a text is not a [`SizeClassName`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SizeClassError {
    /// The text is empty or longer than [`MAX_SIZE_CLASS_LEN`] bytes.
    #[error("a size class is 1 to {MAX_SIZE_CLASS_LEN} bytes")]
    Length,
    /// The text holds a byte outside printable ASCII.
    #[error("a size class holds only printable ASCII (0x20 to 0x7e)")]
    Byte,
}

impl SizeClassError {
    /// The word a refusal names the fault by: `bad-length` or `not-printable-ascii`.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::Length => "bad-length",
            Self::Byte => "not-printable-ascii",
        }
    }
}

/// A job's size class as its client named it: 1 to [`MAX_SIZE_CLASS_LEN`] bytes of printable
/// ASCII, kept byte for byte (see the module docs).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SizeClassName(String);

impl SizeClassName {
    /// The name `text`.
    ///
    /// # Errors
    /// [`SizeClassError::Length`] for an empty text or one over [`MAX_SIZE_CLASS_LEN`]
    /// bytes, else [`SizeClassError::Byte`] for one holding a byte outside `0x20..=0x7e`.
    pub fn parse(text: &str) -> Result<Self, SizeClassError> {
        if text.is_empty() || text.len() > MAX_SIZE_CLASS_LEN {
            return Err(SizeClassError::Length);
        }
        if !text.bytes().all(|byte| matches!(byte, 0x20..=0x7e)) {
            return Err(SizeClassError::Byte);
        }
        Ok(Self(text.to_owned()))
    }

    /// The name, as the client sent it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts that a build target's full name with its kind words is kept byte for byte, at
    /// every length from 1 to the bound, with every printable byte. Catches a name lowered,
    /// cut or rewritten, and a bound or alphabet narrower than the field's.
    #[test]
    fn a_printable_name_of_1_to_512_bytes_is_kept_as_it_is() {
        let printable: String = (0x20u8..=0x7e).map(char::from).collect();
        let long = format!("komira//{}:json mojo_compile build", "src/".repeat(120));
        let longest = "x".repeat(MAX_SIZE_CLASS_LEN);
        for text in [
            "a",
            " ",
            "komira//src/json:json mojo_compile build",
            printable.as_str(),
            &long[..200],
            longest.as_str(),
        ] {
            assert_eq!(SizeClassName::parse(text).unwrap().as_str(), text);
        }
    }

    /// Asserts the two refusals and which wins. Catches an empty name taken as a class, a
    /// name one byte over the bound kept, and a control byte (a line break that would split a
    /// log line, a tab, DEL) or a byte above ASCII let through.
    #[test]
    fn an_empty_or_long_name_or_one_with_a_control_or_non_ascii_byte_is_refused() {
        let over = "x".repeat(MAX_SIZE_CLASS_LEN + 1);
        assert_eq!(SizeClassName::parse(""), Err(SizeClassError::Length));
        assert_eq!(SizeClassName::parse(&over), Err(SizeClassError::Length));
        let over_and_bad = format!("{over}\n");
        assert_eq!(
            SizeClassName::parse(&over_and_bad),
            Err(SizeClassError::Length)
        );
        for bad in ["two\nlines", "tab\there", "nul\0", "del\x7f", "us\x1f", "é"] {
            assert_eq!(
                SizeClassName::parse(bad),
                Err(SizeClassError::Byte),
                "{bad:?}"
            );
        }
    }

    /// Asserts the words a refusal names each fault by, and the messages. Catches a code
    /// changed under a client that matches on it.
    #[test]
    fn each_fault_has_its_code_and_message() {
        assert_eq!(SizeClassError::Length.code(), "bad-length");
        assert_eq!(SizeClassError::Byte.code(), "not-printable-ascii");
        assert_eq!(
            SizeClassError::Length.to_string(),
            "a size class is 1 to 512 bytes"
        );
        assert_eq!(
            SizeClassError::Byte.to_string(),
            "a size class holds only printable ASCII (0x20 to 0x7e)"
        );
    }
}
