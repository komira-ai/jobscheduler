//! Telling the instances this scheduler launched from every other instance of the
//! account. Two markers carry the claim: tags and a name prefix. An instance is ours,
//! and may be terminated, only when both are whole. An instance with some of the marks
//! (a renamed one, one whose tags were replaced) is a suspect: it still bills, so it
//! counts like ours, and nothing touches it.

use js_elastic::{AcquireId, Claim, HostName, SchedulerId};

use crate::launch::{ACQUIRE_TAG, Instance, LABEL_PREFIX, NAME_TAG, SCHEDULER_TAG, Tag};

/// The longest tag key EC2 takes.
const MAX_KEY: usize = 128;

/// The longest tag value EC2 takes.
const MAX_VALUE: usize = 256;

/// The most tags EC2 takes on one instance.
const MAX_TAGS: usize = 50;

/// Why a label cannot become a tag. A label is refused, never rewritten into something
/// else: two labels must not end as one tag.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LabelError {
    /// The key is not lower-case letters, digits and `-` starting with a letter, or the
    /// tag it makes would be longer than 128 characters.
    #[error(
        "label key {0:?} must be lower case letters, digits and '-', starting with a letter, and at most 128 characters as a tag"
    )]
    Key(String),
    /// Two labels have the same key.
    #[error("label key {0:?} is given twice")]
    Twice(String),
    /// The value is longer than 256 characters or not printable ASCII.
    #[error("the value of label {0:?} must be at most 256 printable ASCII characters")]
    Value(String),
    /// The instance would carry more tags than EC2 allows.
    #[error("{0} tags are more than the {MAX_TAGS} allowed")]
    TooMany(usize),
}

/// How this scheduler marks what it launches and recognises it again.
#[derive(Debug, Clone)]
pub(crate) struct Owner {
    scheduler: SchedulerId,
    prefix: String,
}

impl Owner {
    /// The marks of `scheduler`.
    pub(crate) fn new(scheduler: SchedulerId) -> Self {
        let prefix = HostName::prefix(&scheduler);
        Self { scheduler, prefix }
    }

    /// Whether `name` carries this scheduler's prefix, so that an instance launched under
    /// it is recognised as ours afterwards.
    pub(crate) fn owns_name(&self, name: &HostName) -> bool {
        name.as_str().starts_with(&self.prefix)
    }

    /// The tags of the instance launched for `acquire` under `name`: the scheduler, the
    /// acquire and the name, then one per label (`js-label:` and its key).
    pub(crate) fn tags(
        &self,
        acquire: &AcquireId,
        name: &HostName,
        labels: &[(String, String)],
    ) -> Result<Vec<Tag>, LabelError> {
        let tag = |key: &str, value: &str| Tag {
            key: key.to_owned(),
            value: value.to_owned(),
        };
        let mut tags = vec![
            tag(SCHEDULER_TAG, self.scheduler.as_str()),
            tag(ACQUIRE_TAG, acquire.as_str()),
            tag(NAME_TAG, name.as_str()),
        ];
        for (key, value) in labels {
            let made = format!("{LABEL_PREFIX}{key}");
            let letters = key.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
                & key
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
            if !letters | (made.len() > MAX_KEY) {
                return Err(LabelError::Key(key.clone()));
            }
            if tags.iter().any(|t| t.key == made) {
                return Err(LabelError::Twice(key.clone()));
            }
            if (value.len() > MAX_VALUE) | !value.bytes().all(|b| (0x20..=0x7e).contains(&b)) {
                return Err(LabelError::Value(key.clone()));
            }
            tags.push(tag(&made, value));
        }
        match tags.len() > MAX_TAGS {
            true => Err(LabelError::TooMany(tags.len())),
            false => Ok(tags),
        }
    }

    /// What the markers of `instance` say.
    pub(crate) fn claim(&self, instance: &Instance) -> Claim {
        let scheduler = instance.tag(SCHEDULER_TAG);
        let acquire = instance.tag(ACQUIRE_TAG);
        let tagged = scheduler == Some(self.scheduler.as_str());
        let named = instance
            .tag(NAME_TAG)
            .is_some_and(|name| name.starts_with(&self.prefix));
        // An acquire tag with no scheduler tag looks like ours: someone removed half.
        let orphaned = acquire.is_some() & scheduler.is_none();
        match (
            tagged & acquire.is_some() & named,
            tagged | named | orphaned,
        ) {
            (true, _) => Claim::Ours,
            (false, true) => Claim::Suspect,
            (false, false) => Claim::Foreign,
        }
    }

    /// The acquire id in the tags of `instance`, as written.
    pub(crate) fn acquire_of(instance: &Instance) -> Option<&str> {
        instance.tag(ACQUIRE_TAG)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launch::{ImageId, InstanceId, InstanceState};
    use crate::model::{InstanceType, Zone};

    fn owner() -> Owner {
        Owner::new(SchedulerId::new("one").unwrap())
    }

    fn acquire() -> AcquireId {
        AcquireId::new("a").unwrap()
    }

    fn name_of(owner: &Owner) -> HostName {
        HostName::derive(&owner.scheduler, &acquire())
    }

    fn labels(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    fn instance(tags: &[(&str, &str)]) -> Instance {
        Instance {
            id: InstanceId::new("i-0123456789abcdef0").unwrap(),
            state: InstanceState::Running,
            tags: tags
                .iter()
                .map(|(key, value)| Tag {
                    key: (*key).to_owned(),
                    value: (*value).to_owned(),
                })
                .collect(),
            instance_type: InstanceType("m7.large".to_owned()),
            zone: Zone("zone-a1".to_owned()),
            image: ImageId::new("ami-0123456789abcdef0").unwrap(),
        }
    }

    /// Asserts that a host name is owned when it carries this scheduler's prefix and not
    /// when it was derived by another scheduler.
    #[test]
    fn a_name_is_owned_by_the_scheduler_that_derived_it() {
        let one = owner();
        let two = Owner::new(SchedulerId::new("two").unwrap());
        assert!(one.owns_name(&name_of(&one)));
        assert!(!one.owns_name(&name_of(&two)));
    }

    /// Asserts that the tags of an instance are the scheduler, the acquire and the name,
    /// then a `js-label:` tag per label in order, with a label key at its longest and a
    /// value at its longest and empty accepted.
    #[test]
    fn tags_carry_the_scheduler_the_acquire_the_name_and_the_labels() {
        let one = owner();
        let name = name_of(&one);
        let tags = one
            .tags(&acquire(), &name, &labels(&[("pool", "gpu"), ("b2-c", "")]))
            .unwrap();
        let pairs: Vec<(&str, &str)> = tags
            .iter()
            .map(|t| (t.key.as_str(), t.value.as_str()))
            .collect();
        assert_eq!(
            pairs,
            [
                ("js-scheduler", "one"),
                ("js-acquire-id", "a"),
                ("Name", name.as_str()),
                ("js-label:pool", "gpu"),
                ("js-label:b2-c", ""),
            ]
        );
        let longest_key = "k".repeat(MAX_KEY - LABEL_PREFIX.len());
        let longest_value = "v ~".repeat(85) + "v";
        assert_eq!(longest_value.len(), MAX_VALUE);
        let at_the_edge = labels(&[(longest_key.as_str(), longest_value.as_str())]);
        assert!(one.tags(&acquire(), &name, &at_the_edge).is_ok());
    }

    /// Asserts that a label is refused, naming its key, when the key is empty, starts with
    /// a digit, has a capital or a dot, or makes a tag past 128 characters, when the value
    /// is past 256 characters or holds a control or non-ASCII character, and when a key is
    /// given twice.
    #[test]
    fn a_label_that_cannot_be_a_tag_is_refused_by_name() {
        let one = owner();
        let name = name_of(&one);
        let run = |pairs: &[(&str, &str)]| one.tags(&acquire(), &name, &labels(pairs));
        let too_long = "k".repeat(MAX_KEY - LABEL_PREFIX.len() + 1);
        for key in ["", "1a", "A", "a.b", "a_b", too_long.as_str()] {
            assert_eq!(run(&[(key, "v")]), Err(LabelError::Key(key.to_owned())));
        }
        let long_value = "v".repeat(MAX_VALUE + 1);
        for value in [long_value.as_str(), "tab\there", "caf\u{e9}"] {
            assert_eq!(run(&[("k", value)]), Err(LabelError::Value("k".to_owned())));
        }
        assert_eq!(
            run(&[("k", "1"), ("k", "2")]),
            Err(LabelError::Twice("k".to_owned()))
        );
    }

    /// Asserts that 50 tags are accepted (the three of ours and 47 labels) and 51 are
    /// refused with the count.
    #[test]
    fn more_than_fifty_tags_are_refused() {
        let one = owner();
        let name = name_of(&one);
        let many = |count: usize| -> Vec<(String, String)> {
            (0..count)
                .map(|n| (format!("k{n}"), "v".to_owned()))
                .collect()
        };
        assert_eq!(one.tags(&acquire(), &name, &many(47)).unwrap().len(), 50);
        let error = one.tags(&acquire(), &name, &many(48)).unwrap_err();
        assert_eq!(error, LabelError::TooMany(51));
        assert_eq!(error.to_string(), "51 tags are more than the 50 allowed");
    }

    /// Asserts the claim of every combination of the marks: all three whole is ours; the
    /// scheduler tag alone, the name alone, the scheduler tag with the name and no acquire,
    /// the scheduler tag of another scheduler with ours elsewhere, and an acquire tag with
    /// no scheduler tag are suspects; another scheduler's marks, an acquire tag under
    /// another scheduler's tag, and no marks are foreign. It catches a claim that calls a
    /// half-marked instance foreign, which a cap would then not count.
    #[test]
    fn the_claim_depends_on_which_marks_are_whole() {
        let one = owner();
        let mine = name_of(&one).to_string();
        let mine = mine.as_str();
        let theirs = name_of(&Owner::new(SchedulerId::new("two").unwrap())).to_string();
        let theirs = theirs.as_str();
        let cases: [(&[(&str, &str)], Claim); 10] = [
            (
                &[(SCHEDULER_TAG, "one"), (ACQUIRE_TAG, "a"), (NAME_TAG, mine)],
                Claim::Ours,
            ),
            (
                &[(SCHEDULER_TAG, "one"), (ACQUIRE_TAG, "a")],
                Claim::Suspect,
            ),
            (&[(SCHEDULER_TAG, "one"), (NAME_TAG, mine)], Claim::Suspect),
            (&[(SCHEDULER_TAG, "one")], Claim::Suspect),
            (&[(NAME_TAG, mine)], Claim::Suspect),
            (&[(ACQUIRE_TAG, "a")], Claim::Suspect),
            (
                &[
                    (SCHEDULER_TAG, "two"),
                    (ACQUIRE_TAG, "a"),
                    (NAME_TAG, theirs),
                ],
                Claim::Foreign,
            ),
            (
                &[(SCHEDULER_TAG, "two"), (ACQUIRE_TAG, "a")],
                Claim::Foreign,
            ),
            (&[(NAME_TAG, theirs)], Claim::Foreign),
            (&[], Claim::Foreign),
        ];
        for (tags, claim) in cases {
            assert_eq!(one.claim(&instance(tags)), claim, "{tags:?}");
        }
    }

    /// Asserts that the acquire id is read from the acquire tag as written, and that an
    /// instance with no such tag has none.
    #[test]
    fn the_acquire_is_read_from_its_tag() {
        assert_eq!(
            Owner::acquire_of(&instance(&[(ACQUIRE_TAG, "first-1")])),
            Some("first-1")
        );
        assert_eq!(Owner::acquire_of(&instance(&[])), None);
    }
}
