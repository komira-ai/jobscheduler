//! The job row's labels (`job_tag::LABELS`): the caller's labels and the reserved ones the
//! scheduler set, each checked as when it was first made.

use std::collections::BTreeMap;

use js_types::{Labels, RESERVED_PREFIX, ReservedKey};

use super::{DecodeError, text_map_of};

/// The reserved label keys a row may hold.
const RESERVED: [ReservedKey; 5] = [
    ReservedKey::Tenant,
    ReservedKey::Run,
    ReservedKey::ValidationRun,
    ReservedKey::Definition,
    ReservedKey::Instance,
];

/// A label set as a row holds it: the caller's labels, and the reserved ones the scheduler
/// set, each checked as when it was first made. A reserved key this build does not know is
/// one a later build sets: the row is [`DecodeError::Newer`], as for an enum value.
pub(super) fn labels_of(tag: u16, value: Vec<u8>) -> Result<Labels, DecodeError> {
    let bad = DecodeError::BadValue { tag };
    let (reserved, caller): (BTreeMap<String, String>, BTreeMap<String, String>) =
        text_map_of(tag, value)?
            .into_iter()
            .partition(|(key, _)| key.starts_with(RESERVED_PREFIX));
    let labels = Labels::from_caller(&caller).map_err(|_| bad)?;
    reserved
        .into_iter()
        .try_fold(labels, |labels, (key, value)| {
            let known = RESERVED
                .into_iter()
                .find(|reserved| reserved.key() == key)
                .ok_or(DecodeError::Newer)?;
            labels.with_reserved(known, &value).map_err(|_| bad)
        })
}
