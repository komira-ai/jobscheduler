//! Telling the instances this scheduler launched from every other instance of the
//! project. Two markers carry the claim: labels and a name prefix. An instance is ours,
//! and may be deleted, only when both are whole. An instance with some of the marks (a
//! renamed one, one whose labels were replaced) is a suspect: it still bills, so it counts
//! like ours, and nothing touches it.

use js_elastic::{AcquireId, Claim, HostName, SchedulerId};

use crate::launch::{
    ACQUIRE_LABEL, Instance, LABEL_PREFIX, Label, SCHEDULER_LABEL, id_of_label, is_label_key,
    is_label_value, label_value,
};

/// The most labels the cloud takes on one resource.
const MAX_LABELS: usize = 64;

/// Why one of an acquire's labels cannot become a label of the instance. A label is
/// refused, never rewritten into something else: two labels must not end as one.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LabelError {
    /// The key is not lower-case letters, digits, `_` and `-` starting with a letter, or
    /// the label it makes would be longer than 63 characters.
    #[error(
        "label key {0:?} must be lower case letters, digits, '_' and '-', starting with a letter, and at most 63 characters as a label"
    )]
    Key(String),
    /// Two labels have the same key.
    #[error("label key {0:?} is given twice")]
    Twice(String),
    /// The value is longer than 63 characters or holds a character other than lower-case
    /// letters, digits, `_` and `-`.
    #[error(
        "the value of label {0:?} must be at most 63 lower case letters, digits, '_' and '-'"
    )]
    Value(String),
    /// The instance would carry more labels than the cloud allows.
    #[error("{0} labels are more than the {MAX_LABELS} allowed")]
    TooMany(usize),
}

/// How this scheduler marks what it launches and recognises it again.
#[derive(Debug, Clone)]
pub(crate) struct Owner {
    scheduler: String,
    prefix: String,
}

impl Owner {
    /// The marks of `scheduler`.
    pub(crate) fn new(scheduler: &SchedulerId) -> Self {
        Self {
            scheduler: label_value(scheduler.as_str()),
            prefix: HostName::prefix(scheduler),
        }
    }

    /// The value of the scheduler label of everything this scheduler launches.
    pub(crate) fn scheduler_label(&self) -> Label {
        Label {
            key: SCHEDULER_LABEL.to_owned(),
            value: self.scheduler.clone(),
        }
    }

    /// The label that marks what was launched for `acquire`.
    pub(crate) fn acquire_label(acquire: &AcquireId) -> Label {
        Label {
            key: ACQUIRE_LABEL.to_owned(),
            value: label_value(acquire.as_str()),
        }
    }

    /// Whether `name` carries this scheduler's prefix, so that an instance launched under
    /// it is recognised as ours afterwards.
    pub(crate) fn owns_name(&self, name: &str) -> bool {
        name.starts_with(&self.prefix)
    }

    /// The labels of the instance launched for `acquire`: the scheduler and the acquire,
    /// then one per label of the acquire (`js-label-` and its key).
    pub(crate) fn labels(
        &self,
        acquire: &AcquireId,
        labels: &[(String, String)],
    ) -> Result<Vec<Label>, LabelError> {
        let mut made = vec![self.scheduler_label(), Self::acquire_label(acquire)];
        for (key, value) in labels {
            let full = format!("{LABEL_PREFIX}{key}");
            if key.is_empty() | !is_label_key(&full) {
                return Err(LabelError::Key(key.clone()));
            }
            if made.iter().any(|label| label.key == full) {
                return Err(LabelError::Twice(key.clone()));
            }
            if !is_label_value(value) {
                return Err(LabelError::Value(key.clone()));
            }
            made.push(Label {
                key: full,
                value: value.clone(),
            });
        }
        match made.len() > MAX_LABELS {
            true => Err(LabelError::TooMany(made.len())),
            false => Ok(made),
        }
    }

    /// What the markers of `instance` say.
    pub(crate) fn claim(&self, instance: &Instance) -> Claim {
        let scheduler = instance.label(SCHEDULER_LABEL);
        let acquire = instance.label(ACQUIRE_LABEL);
        let labelled = scheduler == Some(self.scheduler.as_str());
        let named = self.owns_name(&instance.name);
        // An acquire label with no scheduler label looks like ours: someone removed half.
        let orphaned = acquire.is_some() & scheduler.is_none();
        match (
            labelled & acquire.is_some() & named,
            labelled | named | orphaned,
        ) {
            (true, _) => Claim::Ours,
            (false, true) => Claim::Suspect,
            (false, false) => Claim::Foreign,
        }
    }

    /// The acquire id in the labels of `instance`, when its acquire label holds it as it is
    /// (a long or upper-case id is held as a hash, which does not give it back).
    pub(crate) fn acquire_of(instance: &Instance) -> Option<&str> {
        instance.label(ACQUIRE_LABEL).and_then(id_of_label)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launch::{InstanceId, InstanceStatus, MAX_NAME};
    use crate::model::{MachineTypeName, Zone};

    fn owner() -> Owner {
        Owner::new(&SchedulerId::new("one").unwrap())
    }

    fn acquire() -> AcquireId {
        AcquireId::new("a").unwrap()
    }

    fn name_of(scheduler: &str) -> String {
        HostName::derive(&SchedulerId::new(scheduler).unwrap(), &acquire()).to_string()
    }

    fn labels(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    fn instance(name: &str, labels: &[(&str, &str)]) -> Instance {
        Instance {
            id: InstanceId::new("1234").unwrap(),
            name: name.to_owned(),
            status: InstanceStatus::Running,
            labels: labels
                .iter()
                .map(|(key, value)| Label {
                    key: (*key).to_owned(),
                    value: (*value).to_owned(),
                })
                .collect(),
            machine_type: MachineTypeName("n2-standard-2".to_owned()),
            zone: Zone::new("zone-one-a").unwrap(),
        }
    }

    /// Asserts that a name is owned when it carries this scheduler's prefix and not when
    /// another scheduler derived it.
    #[test]
    fn a_name_is_owned_by_the_scheduler_that_derived_it() {
        let one = owner();
        assert!(one.owns_name(&name_of("one")));
        assert!(!one.owns_name(&name_of("two")));
    }

    /// Asserts that the labels of an instance are the scheduler and the acquire, then a
    /// `js-label-` label per label in order, with a key at its longest and a value at its
    /// longest and empty accepted.
    #[test]
    fn labels_carry_the_scheduler_the_acquire_and_the_acquires_labels() {
        let one = owner();
        let made = one
            .labels(&acquire(), &labels(&[("pool", "gpu"), ("b2_c", "")]))
            .unwrap();
        let pairs: Vec<(&str, &str)> = made
            .iter()
            .map(|label| (label.key.as_str(), label.value.as_str()))
            .collect();
        assert_eq!(
            pairs,
            [
                ("js-scheduler", "one"),
                ("js-acquire-id", "a"),
                ("js-label-pool", "gpu"),
                ("js-label-b2_c", ""),
            ]
        );
        let longest_key = "k".repeat(MAX_NAME - LABEL_PREFIX.len());
        let longest_value = "v_-9".repeat(15) + "v-9";
        assert_eq!(longest_value.len(), MAX_NAME);
        let edge = labels(&[(longest_key.as_str(), longest_value.as_str())]);
        assert!(one.labels(&acquire(), &edge).is_ok());
    }

    /// Asserts that a label is refused, naming its key, when the key is empty, has a
    /// capital or a dot, or makes a label past 63 characters; when the value is past 63
    /// characters or holds a capital, a dot or a non-ASCII character; and when a key is
    /// given twice.
    #[test]
    fn a_label_that_cannot_be_a_label_is_refused_by_name() {
        let one = owner();
        let run = |pairs: &[(&str, &str)]| one.labels(&acquire(), &labels(pairs));
        let too_long = "k".repeat(MAX_NAME - LABEL_PREFIX.len() + 1);
        for key in ["", "A", "a.b", "a b", too_long.as_str()] {
            assert_eq!(run(&[(key, "v")]), Err(LabelError::Key(key.to_owned())));
        }
        let long_value = "v".repeat(MAX_NAME + 1);
        for value in [long_value.as_str(), "Gpu", "a.b", "caf\u{e9}"] {
            assert_eq!(run(&[("k", value)]), Err(LabelError::Value("k".to_owned())));
        }
        assert_eq!(
            run(&[("k", "1"), ("k", "2")]),
            Err(LabelError::Twice("k".to_owned()))
        );
    }

    /// Asserts that 64 labels are accepted (the two of ours and 62 of the acquire's) and
    /// 65 are refused with the count.
    #[test]
    fn more_than_sixty_four_labels_are_refused() {
        let one = owner();
        let many = |count: usize| -> Vec<(String, String)> {
            (0..count)
                .map(|n| (format!("k{n}"), "v".to_owned()))
                .collect()
        };
        assert_eq!(one.labels(&acquire(), &many(62)).unwrap().len(), 64);
        let error = one.labels(&acquire(), &many(63)).unwrap_err();
        assert_eq!(error, LabelError::TooMany(65));
        assert_eq!(error.to_string(), "65 labels are more than the 64 allowed");
    }

    /// Asserts the claim of every combination of the marks: both labels and the name whole
    /// is ours; the scheduler label alone, the name alone, the scheduler label with the
    /// name and no acquire label, and an acquire label with no scheduler label are
    /// suspects; another scheduler's marks, an acquire label under another scheduler's
    /// label, and no marks are foreign. It catches a claim that calls a half-marked
    /// instance foreign, which the machine cap would then not count.
    #[test]
    fn the_claim_depends_on_which_marks_are_whole() {
        let one = owner();
        let mine = name_of("one");
        let mine = mine.as_str();
        let theirs = name_of("two");
        let theirs = theirs.as_str();
        let cases: [(&str, &[(&str, &str)], Claim); 10] = [
            (
                mine,
                &[(SCHEDULER_LABEL, "one"), (ACQUIRE_LABEL, "a")],
                Claim::Ours,
            ),
            (
                "other",
                &[(SCHEDULER_LABEL, "one"), (ACQUIRE_LABEL, "a")],
                Claim::Suspect,
            ),
            (mine, &[(SCHEDULER_LABEL, "one")], Claim::Suspect),
            ("other", &[(SCHEDULER_LABEL, "one")], Claim::Suspect),
            (mine, &[], Claim::Suspect),
            ("other", &[(ACQUIRE_LABEL, "a")], Claim::Suspect),
            (
                theirs,
                &[(SCHEDULER_LABEL, "two"), (ACQUIRE_LABEL, "a")],
                Claim::Foreign,
            ),
            (
                "other",
                &[(SCHEDULER_LABEL, "two"), (ACQUIRE_LABEL, "a")],
                Claim::Foreign,
            ),
            (theirs, &[], Claim::Foreign),
            ("other", &[], Claim::Foreign),
        ];
        for (name, marks, claim) in cases {
            assert_eq!(one.claim(&instance(name, marks)), claim, "{name} {marks:?}");
        }
    }

    /// Asserts that the acquire id is read from the acquire label when the label holds it
    /// as it is, and that a hashed label and no label give none.
    #[test]
    fn the_acquire_is_read_from_its_label_when_it_holds_it() {
        let plain = instance("n", &[(ACQUIRE_LABEL, "first-1")]);
        assert_eq!(Owner::acquire_of(&plain), Some("first-1"));
        let hashed = label_value("Upper.Case");
        let hashed = instance("n", &[(ACQUIRE_LABEL, hashed.as_str())]);
        assert_eq!(Owner::acquire_of(&hashed), None);
        assert_eq!(Owner::acquire_of(&instance("n", &[])), None);
        let label = Owner::acquire_label(&AcquireId::new("Upper.Case").unwrap());
        assert_eq!(label.value, label_value("Upper.Case"));
    }
}
