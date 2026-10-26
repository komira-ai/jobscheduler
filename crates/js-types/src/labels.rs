use std::collections::BTreeMap;

use crate::ids::is_id_char;

/// The prefix only the scheduler may write (design section 12.5).
pub const RESERVED_PREFIX: &str = "jobscheduler.dev/";

/// Longest label value, in bytes.
const MAX_VALUE_LEN: usize = 63;

/// The reserved label keys the scheduler writes itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ReservedKey {
    /// The party the run is billed against.
    Tenant,
    /// The run id.
    Run,
    /// The validation run id, stamped on every billable resource a run creates.
    ValidationRun,
    /// The definition the run belongs to.
    Definition,
    /// The instance, the key leak sweeps use.
    Instance,
}

impl ReservedKey {
    /// The full label key, prefix included.
    pub fn key(&self) -> &'static str {
        match self {
            Self::Tenant => "jobscheduler.dev/tenant",
            Self::Run => "jobscheduler.dev/run",
            Self::ValidationRun => "jobscheduler.dev/validation-run",
            Self::Definition => "jobscheduler.dev/definition",
            Self::Instance => "jobscheduler.dev/instance",
        }
    }
}

/// A label was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LabelError {
    /// A caller used a key under the reserved prefix.
    #[error("label key {0:?} is under the reserved prefix jobscheduler.dev/")]
    Reserved(String),
    /// A key was empty.
    #[error("label keys must not be empty")]
    EmptyKey,
    /// A value was longer than 63 characters or held a character outside `[a-z0-9._-]`.
    #[error("label {key:?} has a value that is not 1 to 63 characters of [a-z0-9._-]")]
    BadValue {
        /// The key whose value was refused.
        key: String,
    },
}

/// A validated label set, ready to be written onto every billable resource of a run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Labels(BTreeMap<String, String>);

fn check_value(key: &str, value: &str) -> Result<(), LabelError> {
    let in_range = (1..=MAX_VALUE_LEN).contains(&value.len());
    let in_alphabet = value.chars().all(is_id_char);
    if [in_range, in_alphabet].into_iter().all(|ok| ok) {
        Ok(())
    } else {
        Err(LabelError::BadValue {
            key: key.to_string(),
        })
    }
}

impl Labels {
    /// Validates labels a caller supplied: no key under the reserved prefix, no empty
    /// key, every value well formed.
    pub fn from_caller(supplied: &BTreeMap<String, String>) -> Result<Self, LabelError> {
        for (key, value) in supplied {
            if key.is_empty() {
                return Err(LabelError::EmptyKey);
            }
            if key.starts_with(RESERVED_PREFIX) {
                return Err(LabelError::Reserved(key.clone()));
            }
            check_value(key, value)?;
        }
        Ok(Self(supplied.clone()))
    }

    /// These labels with a reserved key set by the scheduler.
    pub fn with_reserved(mut self, key: ReservedKey, value: &str) -> Result<Self, LabelError> {
        check_value(key.key(), value)?;
        self.0.insert(key.key().to_string(), value.to_string());
        Ok(self)
    }

    /// The value under `key`, if set.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }

    /// Every label, in key order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// Asserts that ordinary caller labels are kept and readable.
    #[test]
    fn caller_labels_are_kept() {
        let labels = Labels::from_caller(&map(&[("team", "infra"), ("env", "test")])).unwrap();
        assert_eq!(labels.get("team"), Some("infra"));
        assert_eq!(labels.get("missing"), None);
        assert_eq!(labels.iter().count(), 2);
    }

    /// Asserts that a caller key under the reserved prefix is refused, for every
    /// reserved key and for an unknown key under the prefix.
    #[test]
    fn reserved_prefix_is_refused_from_callers() {
        for key in [
            ReservedKey::Tenant,
            ReservedKey::Run,
            ReservedKey::ValidationRun,
            ReservedKey::Definition,
            ReservedKey::Instance,
        ] {
            let err = Labels::from_caller(&map(&[(key.key(), "x")])).unwrap_err();
            assert_eq!(err, LabelError::Reserved(key.key().to_string()));
        }
        let err = Labels::from_caller(&map(&[("jobscheduler.dev/other", "x")])).unwrap_err();
        assert!(err.to_string().contains("jobscheduler.dev/other"));
    }

    /// Asserts that an empty key, an empty value, an over-long value and a value with
    /// an uppercase letter are refused, and a 63-character value is accepted.
    #[test]
    fn malformed_keys_and_values_are_refused() {
        assert_eq!(
            Labels::from_caller(&map(&[("", "x")])),
            Err(LabelError::EmptyKey)
        );
        let bad = |value: &str| Labels::from_caller(&map(&[("k", value)]));
        let expected = Err(LabelError::BadValue { key: "k".into() });
        assert_eq!(bad(""), expected);
        assert_eq!(bad(&"a".repeat(64)), expected);
        assert_eq!(bad("Up"), expected);
        assert!(bad(&"a".repeat(63)).is_ok());
    }

    /// Asserts that the scheduler can set a reserved key and that its value is
    /// checked like any other.
    #[test]
    fn scheduler_sets_reserved_keys() {
        let labels = Labels::default()
            .with_reserved(ReservedKey::ValidationRun, "run-1")
            .unwrap();
        assert_eq!(labels.get("jobscheduler.dev/validation-run"), Some("run-1"));
        let err = Labels::default()
            .with_reserved(ReservedKey::Run, "NO")
            .unwrap_err();
        assert_eq!(
            err,
            LabelError::BadValue {
                key: "jobscheduler.dev/run".into()
            }
        );
    }
}
