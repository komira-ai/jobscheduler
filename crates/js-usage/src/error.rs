//! What a stored row or a store call can fail with.

use js_state::StoreError;

/// Why the bytes of a stored hour row do not read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CodecError {
    /// Fewer bytes than the version and the count.
    #[error("the row is too short to hold a version and a count")]
    Short,
    /// A version byte this build does not read.
    #[error("the row has version {0}, which this build does not read")]
    Version(u8),
    /// A count outside 1 to 60, or a length that does not match the count.
    #[error("the row's length does not match its count")]
    Length,
    /// A minute of 60 or more within the hour.
    #[error("the row names a minute that is not within an hour")]
    Minute,
    /// Minutes that do not strictly ascend.
    #[error("the row's minutes do not strictly ascend")]
    Order,
}

/// The ways a series call fails. A conflict or a store failure leaves the store as it was.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum UsageError {
    /// The state store failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// A stored row does not read. It is left in place for an operator to look at.
    #[error("row {key} is corrupt: {reason}")]
    Corrupt {
        /// The row's key.
        key: String,
        /// What is wrong with its bytes.
        reason: CodecError,
    },
    /// A wall minute past the last one a row key can name.
    #[error("wall minute {0} is past the last one a series can key")]
    MinuteOutOfRange(u64),
    /// A row changed between its read and its write: another writer is active.
    #[error("row {key} changed under the writer; another writer is active")]
    Conflict {
        /// The row's key.
        key: String,
    },
}

/// A scheduler lease minute whose index does not fit the `u16` of a [`crate::MinuteIn`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "lease minute {minute} does not fit a reading's minute index (at most {})",
    u16::MAX
)]
pub struct MinuteTooLarge {
    /// The minute index given.
    pub minute: u32,
}
