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

macro_rules! name_id {
    ($(#[$doc:meta])* $name:ident, $kind:literal) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
            /// Builds the id, refusing an empty, long or out-of-alphabet value.
            pub fn new(value: &str) -> Result<Self, IdError> {
                check($kind, value).map(Self)
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
        Self(value.to_string())
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
