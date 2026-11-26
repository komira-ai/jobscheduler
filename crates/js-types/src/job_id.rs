//! The id of a job: the client's (1 to 63 bytes of letters, digits, `.`, `_` and `-`) or 128
//! bits the job store mints, written as 32 lowercase hex digits, which is itself a valid job
//! id. It lives here, below every crate that names a job, so the pure cores (the retry
//! decision among them) take the same id the store keys a job's rows by.

use std::fmt;

/// The longest job id, in bytes.
pub const MAX_JOB_ID_LEN: usize = 63;

/// The id of a job: 1 to 63 bytes of ASCII letters, digits, `.`, `_` and `-`. It is the
/// key of the job's row and of its allocation's row as it is, so it never needs escaping.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JobId(String);

/// Text that is not a job id.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("job id {value:?} must be 1 to 63 bytes of letters, digits, '.', '_' and '-'")]
pub struct JobIdError {
    /// The text refused.
    pub value: String,
}

impl JobId {
    /// The job id `text`.
    ///
    /// # Errors
    /// [`JobIdError`] when `text` is empty, longer than [`MAX_JOB_ID_LEN`] bytes, or holds a
    /// byte outside the id alphabet (a non-ASCII character is such a byte).
    pub fn parse(text: &str) -> Result<JobId, JobIdError> {
        let in_range = (1..=MAX_JOB_ID_LEN).contains(&text.len());
        let in_alphabet = text.bytes().all(is_id_byte);
        if [in_range, in_alphabet].into_iter().all(|ok| ok) {
            Ok(JobId(text.to_owned()))
        } else {
            Err(JobIdError {
                value: text.to_owned(),
            })
        }
    }

    /// The id minted from `bits`: 32 lowercase hex digits.
    #[must_use]
    pub fn minted(bits: [u8; 16]) -> JobId {
        JobId(hex(bits))
    }

    /// The id as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for JobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Whether `byte` is in the job id alphabet.
fn is_id_byte(byte: u8) -> bool {
    matches!(byte, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'.' | b'_' | b'-')
}

/// `bits` as 32 lowercase hex digits.
fn hex(bits: [u8; 16]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(32);
    for byte in bits {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALLOWED: &[u8] = b"abcxyzABCXYZ0189._-";

    // Catches a length bound off by one at either end, and an alphabet that admits a
    // separator, a space or a byte of a multi-byte character.
    #[test]
    fn job_id_grammar_is_enforced() {
        let one = "a".to_owned();
        let longest: String = ALLOWED
            .iter()
            .cycle()
            .take(63)
            .map(|b| char::from(*b))
            .collect();
        for good in [one.as_str(), longest.as_str(), "Job.1_a-Z"] {
            assert_eq!(
                JobId::parse(good).map(|id| id.to_string()),
                Ok(good.to_owned())
            );
        }
        let too_long = "a".repeat(64);
        for bad in ["", too_long.as_str(), "a/b", "a b", "caf\u{e9}"] {
            assert_eq!(
                JobId::parse(bad),
                Err(JobIdError {
                    value: bad.to_owned()
                }),
                "{bad:?} must be refused"
            );
        }
        let said = JobIdError {
            value: "a b".into(),
        }
        .to_string();
        assert_eq!(
            said,
            "job id \"a b\" must be 1 to 63 bytes of letters, digits, '.', '_' and '-'"
        );
    }

    // Catches minted text that is not 32 lowercase hex digits in byte order, or that is
    // not itself a job id.
    #[test]
    fn minted_ids_are_32_lowercase_hex_digits() {
        let bits = [
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54,
            0x32, 0x10,
        ];
        let id = JobId::minted(bits);
        assert_eq!(id.as_str(), "0123456789abcdeffedcba9876543210");
        assert_eq!(JobId::parse(id.as_str()), Ok(id));
    }
}
