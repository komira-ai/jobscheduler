//! The one error every `validate` returns.

/// Which rule a field broke. The text is fixed: it never holds a byte of the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Rule {
    /// The field is empty.
    #[error("must not be empty")]
    Empty,
    /// The field is longer than its cap.
    #[error("is longer than the cap")]
    TooLong,
    /// A list or map holds more entries than its cap.
    #[error("holds more entries than the cap")]
    TooMany,
    /// The field holds a character its alphabet does not allow.
    #[error("holds a character that is not allowed")]
    BadChars,
    /// A number is outside its range.
    #[error("is outside its range")]
    OutOfRange,
    /// An entry repeats another entry of the same list.
    #[error("repeats an entry")]
    Duplicate,
    /// The field contradicts another field of the same message.
    #[error("contradicts another field")]
    Inconsistent,
    /// A label key uses the prefix only the scheduler may write.
    #[error("uses the reserved label prefix")]
    Reserved,
    /// A path leaves the root it is relative to.
    #[error("leaves its root")]
    Escapes,
    /// An image reference is not pinned by a `sha256:` digest of 64 lower case hex digits.
    #[error("must be pinned by a sha256 digest")]
    NotPinned,
    /// The peer speaks no protocol version this side speaks.
    #[error("names no protocol version this side speaks")]
    UnsupportedProtocol,
}

/// Why a message is refused: the field, the rule it broke and the refusal code for the
/// wire.
///
/// `Display` is a fixed text built from the field's name and the rule, never from the
/// peer's data, so a log line or a reply built from it stays small however large the
/// offending value was. [`ValidationError::code`] is the stable output a peer matches
/// on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{field} {rule}")]
pub struct ValidationError {
    field: &'static str,
    rule: Rule,
    code: &'static str,
}

impl ValidationError {
    pub(crate) fn new(field: &'static str, rule: Rule, code: &'static str) -> Self {
        Self { field, rule, code }
    }

    /// The refusal code that goes on the wire for this error; one of [`crate::refusal`].
    pub fn code(&self) -> &'static str {
        self.code
    }

    /// The name of the field that was refused, such as `labels` or `mounts.rel`.
    pub fn field(&self) -> &'static str {
        self.field
    }

    /// The rule the field broke.
    pub fn rule(&self) -> Rule {
        self.rule
    }
}
