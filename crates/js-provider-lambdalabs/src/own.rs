//! Telling the instances this scheduler launched from every other instance of the
//! account. Two markers carry the claim: tags and a name prefix. An instance is ours,
//! and may be terminated, only when both are whole. An instance with some of the marks
//! (a renamed one, one whose tags were replaced) is a suspect: it still bills, so it
//! counts like ours, and nothing touches it.

use js_elastic::{AcquireId, Claim, HostName, SchedulerId};

use crate::model::{Instance, Tag};

/// The tag that holds the id of the scheduler that launched the instance.
pub const SCHEDULER_TAG: &str = "jobscheduler:scheduler";
/// The tag that holds the id of the acquire the instance was launched for.
pub const ACQUIRE_TAG: &str = "jobscheduler:acquire";
/// The tag that holds the name of the pool.
pub const POOL_TAG: &str = "jobscheduler:pool";

/// The prefix of the key of a tag made from a label.
const LABEL_PREFIX: &str = "jobscheduler:";

/// The longest tag key Lambda takes.
const MAX_KEY: usize = 55;

/// The longest tag value Lambda takes.
const MAX_VALUE: usize = 128;

/// The most tags one instance gets. Lambda documents no count; the launch of an instance
/// with too many would fail, so the crate stops short of finding out.
pub const MAX_TAGS: usize = 16;

/// Why a label or the pool name cannot become a tag. A label is refused, never rewritten
/// into something else: two labels must not end as one tag.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LabelError {
    /// The key is not lower case letters, digits and `-` starting with a letter, or the
    /// tag it makes would be too long.
    #[error("label key {0:?} must be lower case letters, digits and '-', starting with a letter")]
    Key(String),
    /// The key is one the crate sets itself.
    #[error("label key {0:?} is reserved")]
    Reserved(String),
    /// Two labels have the same key.
    #[error("label key {0:?} is given twice")]
    Twice(String),
    /// The value is empty, longer than 128 characters or not printable ASCII.
    #[error("the value of label {0:?} must be 1 to 128 printable ASCII characters")]
    Value(String),
    /// The instance would carry more than the most tags the crate sets.
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
    pub(crate) fn new(scheduler: SchedulerId) -> Self {
        let prefix = HostName::prefix(&scheduler);
        Self { scheduler, prefix }
    }

    /// Whether `name` carries this scheduler's prefix, so that an instance launched
    /// under it is recognised as ours afterwards.
    pub(crate) fn owns_name(&self, name: &HostName) -> bool {
        name.as_str().starts_with(&self.prefix)
    }

    /// The tags of the instance launched for `acquire` in `pool`, with one more per
    /// label.
    pub(crate) fn tags(
        &self,
        acquire: &AcquireId,
        pool: &str,
        labels: &[(String, String)],
    ) -> Result<Vec<Tag>, LabelError> {
        let tag = |key: &str, value: &str| Tag {
            key: key.to_owned(),
            value: value.to_owned(),
        };
        let value_ok = |text: &str| {
            (1..=MAX_VALUE).contains(&text.len()) && text.bytes().all(|b| b.is_ascii_graphic())
        };
        if !value_ok(pool) {
            return Err(LabelError::Value("pool".to_owned()));
        }
        let mut tags = vec![
            tag(SCHEDULER_TAG, self.scheduler.as_str()),
            tag(ACQUIRE_TAG, acquire.as_str()),
            tag(POOL_TAG, pool),
        ];
        for (key, value) in labels {
            let made = format!("{LABEL_PREFIX}{key}");
            let letters = key.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
                && key
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
            if !letters || made.len() > MAX_KEY {
                return Err(LabelError::Key(key.clone()));
            }
            if tags.iter().any(|t| t.key == made) {
                return Err(
                    match matches!(key.as_str(), "scheduler" | "acquire" | "pool") {
                        true => LabelError::Reserved(key.clone()),
                        false => LabelError::Twice(key.clone()),
                    },
                );
            }
            if !value_ok(value) {
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
        let tag = |key: &str| {
            instance
                .tags
                .iter()
                .find(|t| t.key == key)
                .map(|t| t.value.as_str())
        };
        let scheduler = tag(SCHEDULER_TAG);
        let acquire = tag(ACQUIRE_TAG);
        let tagged = scheduler == Some(self.scheduler.as_str());
        let named = instance
            .name
            .as_deref()
            .is_some_and(|name| name.starts_with(&self.prefix));
        // An acquire tag with no scheduler tag is ours-looking: someone removed half.
        let orphaned_acquire = acquire.is_some() && scheduler.is_none();
        match (
            tagged && acquire.is_some() && named,
            tagged || named || orphaned_acquire,
        ) {
            (true, _) => Claim::Ours,
            (false, true) => Claim::Suspect,
            (false, false) => Claim::Foreign,
        }
    }

    /// The acquire id in the tags of `instance`, as written.
    pub(crate) fn acquire_of(instance: &Instance) -> Option<&str> {
        instance
            .tags
            .iter()
            .find(|t| t.key == ACQUIRE_TAG)
            .map(|t| t.value.as_str())
    }
}
