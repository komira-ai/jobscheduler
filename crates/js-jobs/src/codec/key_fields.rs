//! The fields that name a job's work: its labels (tag 20, `job_tag::LABELS`, a map) and its
//! history key ([`job_key_tag::HISTORY_KEY`], text), with the same additive rules
//! (B8) as every other field.
//!
//! The history key is written only for a job that sends one, so the row of a job that sends
//! none has the bytes a build before the field wrote, and a row without the tag reads as none.
//! A build from before the tag skips it as it skips any tag it does not know and carries it
//! into its next write of the row ([`super::Carried`]); it sizes the job by its labels alone. A
//! value that is not a [`HistoryKey`] (empty, over 512 bytes, or a byte outside printable
//! ASCII) is [`DecodeError::BadValue`], as a refused label is.

use js_types::{HistoryKey, Labels};

use super::{DecodeError, Fields, Writer, job_tag, map, text_of};

/// The tag the history key adds to a job row, after the ones of `job_tag` and the other
/// `*_fields` modules.
pub(crate) mod job_key_tag {
    /// The job's history key (text, 1 to 512 bytes of printable ASCII).
    pub const HISTORY_KEY: u16 = 66;
}

/// Puts `labels` and `history_key` in `row`: the labels always (an empty map when there are
/// none), the history key only when the job names one.
pub(super) fn put(row: &mut Writer, labels: &Labels, history_key: Option<&HistoryKey>) {
    row.put(
        job_tag::LABELS,
        map(labels.iter().map(|(k, v)| (k.as_bytes(), v.as_bytes()))),
    );
    row.put_opt(
        job_key_tag::HISTORY_KEY,
        history_key.map(|class| class.as_str().as_bytes().to_vec()),
    );
}

/// The history key of a row: none when the row lacks the tag.
pub(super) fn take_history_key(fields: &mut Fields) -> Result<Option<HistoryKey>, DecodeError> {
    fields.opt(job_key_tag::HISTORY_KEY, history_key_of)
}

/// The history key a value holds.
fn history_key_of(tag: u16, value: Vec<u8>) -> Result<HistoryKey, DecodeError> {
    HistoryKey::parse(&text_of(tag, value)?).map_err(|_| DecodeError::BadValue { tag })
}

#[cfg(test)]
#[path = "key_fields_tests.rs"]
mod tests;
