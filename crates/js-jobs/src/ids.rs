//! The ids of jobs and allocations, and where minted ids come from.
//!
//! A job id is the client's (1 to 63 bytes of letters, digits, `.`, `_` and `-`) or 128 bits
//! the store mints, written as 32 lowercase hex digits, which is itself a valid job id. An
//! allocation id is 128 bits minted at every placement. Minted bits come from an [`IdSource`]
//! the store is given: [`RandomIds`] in a service, a scripted source in tests. Nothing here
//! keeps a counter, so no id depends on how often a process has started.

use std::fmt;
use std::num::NonZeroU32;
#[cfg(any(test, feature = "test-support"))]
use std::sync::{Mutex, PoisonError};

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

/// The id of one allocation: 128 bits minted when the job is placed. A job placed again
/// after a requeue gets a new one, so the job id with the allocation id names one placement
/// and a late report about an earlier one cannot be taken for the current one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AllocationId([u8; 16]);

impl AllocationId {
    /// The allocation id of `bits`.
    #[must_use]
    pub const fn from_bits(bits: [u8; 16]) -> AllocationId {
        AllocationId(bits)
    }

    /// The 128 bits.
    #[must_use]
    pub const fn bits(&self) -> [u8; 16] {
        self.0
    }
}

impl fmt::Display for AllocationId {
    /// 32 lowercase hex digits.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex(self.0))
    }
}

/// The system's random source failed to give bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the system random source failed (code {code})")]
pub struct MintError {
    /// The source's error code.
    pub code: NonZeroU32,
}

/// Where the store's minted ids come from.
pub trait IdSource: Send + Sync {
    /// 128 bits no earlier call of any source has returned, as far as the source can tell.
    ///
    /// # Errors
    /// [`MintError`] when the source cannot give them.
    fn mint(&self) -> Result<[u8; 16], MintError>;
}

/// 128 bits from the operating system's random source per id: two stores, two processes or
/// two boots of one service mint the same id with odds of about one in 2^64 after 2^32 ids.
#[derive(Debug, Clone, Copy, Default)]
pub struct RandomIds;

impl IdSource for RandomIds {
    fn mint(&self) -> Result<[u8; 16], MintError> {
        from_draw(draw())
    }
}

/// 16 bytes from the operating system.
fn draw() -> Result<[u8; 16], getrandom::Error> {
    let mut bits = [0u8; 16];
    getrandom::getrandom(&mut bits).map(|()| bits)
}

/// A draw as the source answers it.
fn from_draw(drawn: Result<[u8; 16], getrandom::Error>) -> Result<[u8; 16], MintError> {
    drawn.map_err(|error| MintError { code: error.code() })
}

/// A source that returns the ids it was given, in order, starting over after the last, so a
/// test decides which mint collides. Test support.
#[cfg(any(test, feature = "test-support"))]
#[derive(Debug)]
pub struct ScriptedIds {
    script: Vec<[u8; 16]>,
    next: Mutex<usize>,
}

#[cfg(any(test, feature = "test-support"))]
impl ScriptedIds {
    /// A source that returns `first`, then each of `rest`, then starts over.
    #[must_use]
    pub fn new(first: [u8; 16], rest: impl IntoIterator<Item = [u8; 16]>) -> ScriptedIds {
        ScriptedIds {
            script: std::iter::once(first).chain(rest).collect(),
            next: Mutex::new(0),
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
impl IdSource for ScriptedIds {
    fn mint(&self) -> Result<[u8; 16], MintError> {
        let mut next = self.next.lock().unwrap_or_else(PoisonError::into_inner);
        let bits = self.script[*next % self.script.len()];
        *next += 1;
        Ok(bits)
    }
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
        let allocation = AllocationId::from_bits(bits);
        assert_eq!(allocation.bits(), bits);
        assert_eq!(allocation.to_string(), "0123456789abcdeffedcba9876543210");
    }

    // Catches a source that hands out the same bits twice, or bits of the wrong length.
    #[test]
    fn random_ids_differ_from_draw_to_draw() {
        let first = RandomIds.mint().expect("the system random source works");
        let second = RandomIds.mint().expect("the system random source works");
        assert_ne!(first, second);
    }

    // Catches a failed draw turned into bits (all zeros, say) instead of an error.
    #[test]
    fn a_failed_draw_is_an_error_with_its_code() {
        let failed = from_draw(Err(getrandom::Error::UNEXPECTED));
        assert_eq!(
            failed,
            Err(MintError {
                code: getrandom::Error::UNEXPECTED.code()
            })
        );
        let said = failed.expect_err("a failed draw").to_string();
        assert!(
            said.starts_with("the system random source failed (code "),
            "{said}"
        );
    }

    // Catches a script that does not start over after its last id.
    #[test]
    fn a_script_repeats_from_its_start() {
        let script = ScriptedIds::new([1; 16], [[2; 16]]);
        let minted: Vec<[u8; 16]> = (0..3).map(|_| script.mint().expect("scripted")).collect();
        assert_eq!(minted, [[1; 16], [2; 16], [1; 16]]);
    }
}
