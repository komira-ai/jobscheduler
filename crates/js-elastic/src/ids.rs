//! The ids and names a scheduler and a provider agree on.

use std::fmt;

use sha2::{Digest, Sha256};

/// The longest acquire id.
const MAX_ACQUIRE: usize = 128;

/// The longest scheduler id.
const MAX_SCHEDULER: usize = 64;

/// Characters of a hash kept in a name.
const HASH_CHARS: usize = 12;

/// Text that is not an id.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("an id is 1 to {max} letters, digits, '.', '_' or '-'")]
pub struct InvalidId {
    max: usize,
}

fn valid_id(text: &str, max: usize) -> Result<String, InvalidId> {
    let fits = (1..=max).contains(&text.len())
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    match fits {
        true => Ok(text.to_owned()),
        false => Err(InvalidId { max }),
    }
}

/// The id of a scheduler: 1 to 64 letters, digits, `.`, `_` or `-`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SchedulerId(String);

impl SchedulerId {
    /// The id `text`.
    ///
    /// # Errors
    ///
    /// [`InvalidId`] when `text` is not an id.
    pub fn new(text: &str) -> Result<Self, InvalidId> {
        valid_id(text, MAX_SCHEDULER).map(Self)
    }

    /// The id as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The id of an acquire: 1 to 128 letters, digits, `.`, `_` or `-`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AcquireId(String);

impl AcquireId {
    /// The id `text`.
    ///
    /// # Errors
    ///
    /// [`InvalidId`] when `text` is not an id.
    pub fn new(text: &str) -> Result<Self, InvalidId> {
        valid_id(text, MAX_ACQUIRE).map(Self)
    }

    /// The id as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AcquireId {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(&self.0)
    }
}

fn short_hash(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    let mut hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    hex.truncate(HASH_CHARS);
    hex
}

/// The name of a host, chosen before it is launched. It is derived from the scheduler id
/// and the acquire id, so the same acquire always gets the same name, and it starts with
/// a prefix that only this scheduler's hosts carry: a provider that marks what it
/// launches by name recognises its own by [`HostName::prefix`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct HostName(String);

impl HostName {
    /// The prefix of every host name of `scheduler`: `js-` and a short hash of its id.
    pub fn prefix(scheduler: &SchedulerId) -> String {
        format!("js-{}-", short_hash(scheduler.as_str()))
    }

    /// The name `text` if it is one `scheduler` derives: its prefix and 12 lower-case
    /// hexadecimal digits. A name read back from a record or a provider, never a way to
    /// make one up.
    pub fn parse(scheduler: &SchedulerId, text: &str) -> Option<Self> {
        let rest = text.strip_prefix(Self::prefix(scheduler).as_str())?;
        let digits = rest.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        (digits & (rest.len() == HASH_CHARS)).then_some(Self(text.to_owned()))
    }

    /// The name of the host launched for `acquire` by `scheduler`: the prefix and a short
    /// hash of the acquire id.
    pub fn derive(scheduler: &SchedulerId, acquire: &AcquireId) -> Self {
        Self(format!(
            "{}{}",
            Self::prefix(scheduler),
            short_hash(acquire.as_str())
        ))
    }

    /// The name as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for HostName {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(&self.0)
    }
}

/// The provider's own id for a machine, opaque to the core.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct HostRef(pub String);

impl HostRef {
    /// The id as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for HostRef {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts that an id is accepted exactly when it is 1 to its limit of letters,
    /// digits, `.`, `_` and `-`, that the error names the limit, and that an id reads
    /// back as given.
    #[test]
    fn ids_are_checked() {
        assert_eq!(
            SchedulerId::new("sched.one_2-x").unwrap().as_str(),
            "sched.one_2-x"
        );
        assert!(SchedulerId::new(&"s".repeat(64)).is_ok());
        let long = "s".repeat(65);
        for bad in ["", "has space", "slash/", "a:b", "é", long.as_str()] {
            assert!(SchedulerId::new(bad).is_err(), "{bad:?}");
        }
        assert!(AcquireId::new(&"a".repeat(128)).is_ok());
        assert!(AcquireId::new(&"a".repeat(129)).is_err());
        let err = AcquireId::new("").unwrap_err();
        assert_eq!(
            err.to_string(),
            "an id is 1 to 128 letters, digits, '.', '_' or '-'"
        );
        let acquire = AcquireId::new("first-launch-ab12").unwrap();
        assert_eq!(acquire.as_str(), "first-launch-ab12");
        assert_eq!(acquire.to_string(), "first-launch-ab12");
    }

    /// Asserts that a host name is the scheduler's prefix and a hash of the acquire id,
    /// that one acquire always gets one name, that two acquires get two, and that two
    /// schedulers never share a prefix.
    #[test]
    fn a_host_name_is_derived_from_the_scheduler_and_the_acquire() {
        let one = SchedulerId::new("one").unwrap();
        let two = SchedulerId::new("two").unwrap();
        let a = AcquireId::new("a").unwrap();
        let b = AcquireId::new("b").unwrap();
        let name = HostName::derive(&one, &a);
        assert_eq!(name, HostName::derive(&one, &a));
        assert_ne!(name, HostName::derive(&one, &b));
        assert_ne!(name, HostName::derive(&two, &a));
        let prefix = HostName::prefix(&one);
        assert!(name.as_str().starts_with(&prefix));
        assert!(!HostName::derive(&two, &a).as_str().starts_with(&prefix));
        // "js-", 12 hex digits, "-", 12 hex digits.
        assert_eq!(prefix.len(), 16);
        assert_eq!(name.as_str().len(), 28);
        assert_eq!(name.to_string(), name.as_str());
    }

    /// Asserts that a name this scheduler derives reads back as itself, and that text
    /// that is not one does not: another scheduler's name, a name with a short, long or
    /// upper-case tail, and no name at all. It catches a parser that takes any text with
    /// the prefix, which would let a record or a provider's answer make up a name.
    #[test]
    fn only_a_derived_name_reads_back() {
        let one = SchedulerId::new("one").unwrap();
        let two = SchedulerId::new("two").unwrap();
        let name = HostName::derive(&one, &AcquireId::new("a").unwrap());
        assert_eq!(HostName::parse(&one, name.as_str()), Some(name.clone()));
        assert_eq!(HostName::parse(&two, name.as_str()), None);
        let prefix = HostName::prefix(&one);
        for tail in ["", "abc", "0123456789abc", "0123456789AB", "0123456789xy"] {
            assert_eq!(
                HostName::parse(&one, &format!("{prefix}{tail}")),
                None,
                "{tail}"
            );
        }
        assert_eq!(HostName::parse(&one, ""), None);
    }

    /// Asserts that a host reference keeps the provider's text and prints it.
    #[test]
    fn a_host_ref_keeps_the_providers_text() {
        let reference = HostRef("i-0abc".to_owned());
        assert_eq!(reference.as_str(), "i-0abc");
        assert_eq!(reference.to_string(), "i-0abc");
    }
}
