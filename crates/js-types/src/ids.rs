use std::borrow::Cow;
use std::fmt;

/// Longest id, in bytes.
const MAX_ID_LEN: usize = 63;

/// An id was empty, too long, or held a character outside `[a-z0-9._-]`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{kind} id {value:?} must be 1 to 63 characters of [a-z0-9._-]")]
pub struct IdError {
    kind: &'static str,
    value: String,
}

fn check(kind: &'static str, value: &str) -> Result<String, IdError> {
    let in_range = (1..=MAX_ID_LEN).contains(&value.len());
    let in_alphabet = value.chars().all(is_id_char);
    if [in_range, in_alphabet].into_iter().all(|ok| ok) {
        Ok(value.to_string())
    } else {
        Err(IdError {
            kind,
            value: value.to_string(),
        })
    }
}

/// Whether `c` is in the id alphabet `[a-z0-9._-]`.
pub(crate) fn is_id_char(c: char) -> bool {
    matches!(c, 'a'..='z' | '0'..='9' | '.' | '_' | '-')
}

/// Whether `text` is an id: 1 to 63 bytes of `[a-z0-9._-]`. A `const fn`, so that an id
/// written in code is checked when the program is built.
const fn is_id(text: &[u8]) -> bool {
    let mut fits = (!text.is_empty()) & (text.len() <= MAX_ID_LEN);
    let mut at = 0;
    while at < text.len() {
        fits &= matches!(text[at], b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-');
        at += 1;
    }
    fits
}

macro_rules! name_id {
    ($(#[$doc:meta])* $name:ident, $kind:literal) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(Cow<'static, str>);

        impl $name {
            /// Builds the id, refusing an empty, long or out-of-alphabet value.
            pub fn new(value: &str) -> Result<Self, IdError> {
                check($kind, value).map(|value| Self(Cow::Owned(value)))
            }

            /// The id as text.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

name_id!(
    /// The id of a resource class, such as `gpu-host`.
    ClassId,
    "class"
);
name_id!(
    /// The id of one host of a pool.
    HostId,
    "host"
);
name_id!(
    /// The id of a provider. The scheduler never branches on its text.
    ProviderId,
    "provider"
);

impl ClassId {
    /// An id from a constant already known to be valid.
    pub(crate) fn from_trusted(value: &str) -> Self {
        Self(Cow::Owned(value.to_string()))
    }
}

impl ProviderId {
    /// The id `text`, a name written in a provider's code, such as its kind. In a `const`
    /// item, text that is not an id fails the build, so a provider names itself with no
    /// error to handle.
    ///
    /// # Panics
    ///
    /// When `text` is not an id and this is called outside a `const` item.
    pub const fn known(text: &'static str) -> Self {
        assert!(
            is_id(text.as_bytes()),
            "a provider id is 1 to 63 characters of [a-z0-9._-]"
        );
        Self(Cow::Borrowed(text))
    }
}

/// A request id was empty, too long, or held a character outside `[a-z0-9._-]`. The
/// text that was refused is not kept, so the error can be logged or sent back as it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("a request id must be 1 to 63 characters of [a-z0-9._-]")]
pub struct RequestIdError;

/// The id a consumer gives a request and presents again on every poll, so the registry
/// can tell a repeat of a request from a new one. The scheduler never branches on its
/// text.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RequestId(String);

impl RequestId {
    /// Builds the id, refusing an empty, long or out-of-alphabet value without keeping
    /// it.
    pub fn new(value: &str) -> Result<Self, RequestIdError> {
        check("request", value)
            .map(Self)
            .map_err(|_| RequestIdError)
    }

    /// The id as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The scheduler's grant of resources to one attempt, identified by (term, sequence)
/// as in the design's terms (section 3). Two grants never share a pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LeaseId {
    term: u64,
    sequence: u64,
}

impl LeaseId {
    /// The lease with this term and sequence.
    pub fn new(term: u64, sequence: u64) -> Self {
        Self { term, sequence }
    }

    /// The scheduler term that granted the lease.
    pub fn term(&self) -> u64 {
        self.term
    }

    /// The sequence number within the term.
    pub fn sequence(&self) -> u64 {
        self.sequence
    }
}

impl fmt::Display for LeaseId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.term, self.sequence)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts that a well-formed id is accepted and read back unchanged.
    #[test]
    fn accepts_a_valid_id() {
        let id = HostId::new("gpu-01.a_b").unwrap();
        assert_eq!(id.as_str(), "gpu-01.a_b");
        assert_eq!(id.to_string(), "gpu-01.a_b");
    }

    /// Asserts that an empty id, a 64-character id and an uppercase id are refused,
    /// and that a 63-character id is accepted.
    #[test]
    fn refuses_empty_long_and_out_of_alphabet_ids() {
        assert!(ClassId::new("").is_err());
        assert!(ClassId::new(&"a".repeat(64)).is_err());
        assert!(ClassId::new(&"a".repeat(63)).is_ok());
        let err = ProviderId::new("Pool").unwrap_err();
        assert!(err.to_string().starts_with("provider id \"Pool\""));
    }

    /// Asserts that a provider id written in code is the same id as the same text read at
    /// run time, and is checked as `new` checks: every character of the alphabet and the
    /// longest length pass. It catches a named id that compares unequal to a parsed one.
    #[test]
    fn a_named_provider_id_is_the_parsed_one() {
        const LAMBDA: ProviderId = ProviderId::known("lambdalabs");
        assert_eq!(LAMBDA, ProviderId::new("lambdalabs").unwrap());
        assert_eq!(ProviderId::known("fake").as_str(), "fake");
        let long = "a".repeat(63);
        let every = ["a-z.0_9", long.as_str()];
        for text in every {
            let leaked: &'static str = Box::leak(text.to_owned().into_boxed_str());
            assert_eq!(ProviderId::known(leaked), ProviderId::new(text).unwrap());
        }
    }

    /// Asserts that a provider id written in code that is not an id is refused: too long,
    /// empty, or out of the alphabet. In a `const` item the same text fails the build.
    #[test]
    fn a_named_provider_id_that_is_not_an_id_is_refused() {
        let long: &'static str = Box::leak("a".repeat(64).into_boxed_str());
        for bad in ["", "Pool", "a b", long] {
            let refused = std::panic::catch_unwind(|| ProviderId::known(bad));
            assert!(refused.is_err(), "{bad:?}");
            assert!(ProviderId::new(bad).is_err(), "{bad:?}");
        }
    }

    /// Asserts that a request id takes the id alphabet and length, and that the error
    /// for a refused one does not carry the text it refused.
    #[test]
    fn request_ids_are_checked_and_the_error_keeps_no_input() {
        let id = RequestId::new("req-1.a_b").unwrap();
        assert_eq!(
            (id.as_str(), id.to_string().as_str()),
            ("req-1.a_b", "req-1.a_b")
        );
        assert!(RequestId::new(&"a".repeat(63)).is_ok());
        assert_eq!(RequestId::new(""), Err(RequestIdError));
        for bad in ["BAD ID", "x y z", "q/q", &"a".repeat(64)] {
            let err = RequestId::new(bad).unwrap_err();
            assert_eq!(err, RequestIdError);
            let shown = format!("{err} {err:?}");
            assert!(!shown.contains(bad), "{shown}");
        }
    }

    /// Asserts that lease ids order by term and then sequence and print as
    /// `term-sequence`.
    #[test]
    fn lease_ids_order_and_print() {
        let a = LeaseId::new(1, 9);
        let b = LeaseId::new(2, 0);
        assert!(a < b);
        assert_eq!((b.term(), a.sequence()), (2, 9));
        assert_eq!(a.to_string(), "1-9");
    }
}
