//! The bytes of a mark row (`mark/<id>`, [`MarkRecord`]): the same format byte and
//! tag-length-value fields as a job or allocation row, with the tags of [`mark_tag`], and the
//! same additive rules (B8): a tag this build does not know is carried, a known tag the row
//! lacks reads as its default, and a format byte or an exclusion kind this build does not know
//! is [`DecodeError::Newer`].
//!
//! The exclusions are a map of host to a value of one kind byte (0 a back-off, 1 a hard
//! exclusion until an instant, 2 one for life) followed, for the first two, by the time it
//! ends. The same encoding of a map of exclusions is the value of a job row's prepare bars
//! (`whole_fields`).

use std::collections::BTreeMap;

use js_types::HostId;

use super::{
    DecodeError, Fields, Writer, host_of, job_id_of, map, no_repeats, pairs_of, text, text_of,
    time, time_of,
};
use crate::error::JobStoreError;
use crate::record::{Exclusion, MarkRecord};

/// The tags of a mark row.
pub(crate) mod mark_tag {
    /// The job (text).
    pub const JOB: u16 = 1;
    /// The held host (text).
    pub const HOST: u16 = 2;
    /// When the mark was made (time).
    pub const MADE: u16 = 3;
    /// When the host is expected to be drained at the latest (time).
    pub const DEADLINE: u16 = 4;
    /// The end of the host's tail when the mark was made (time).
    pub const TAIL_END: u16 = 5;
    /// The hosts the job may not mark (map of text to a kind byte and, but for a kind of 2,
    /// a time).
    pub const EXCLUSIONS: u16 = 6;
}

use mark_tag as t;

/// The kind byte of a back-off.
const BACKOFF: u8 = 0;
/// The kind byte of a hard exclusion until an instant.
const UNTIL: u8 = 1;
/// The kind byte of a hard exclusion for life.
const FOR_LIFE: u8 = 2;

/// The value of one exclusion.
fn exclusion(excluded: Exclusion) -> Vec<u8> {
    let (kind, ends) = match excluded {
        Exclusion::Backoff(ends) => (BACKOFF, Some(ends)),
        Exclusion::Until(ends) => (UNTIL, Some(ends)),
        Exclusion::ForLife => (FOR_LIFE, None),
    };
    let mut out = vec![kind];
    out.extend(ends.map(time).unwrap_or_default());
    out
}

/// The exclusion a value holds: a kind this build does not know is a later build's.
fn exclusion_of(tag: u16, value: &[u8]) -> Result<Exclusion, DecodeError> {
    let (&kind, rest) = value.split_first().ok_or(DecodeError::BadLength { tag })?;
    let ends = || time_of(tag, rest.to_vec());
    match kind {
        BACKOFF => ends().map(Exclusion::Backoff),
        UNTIL => ends().map(Exclusion::Until),
        FOR_LIFE if rest.is_empty() => Ok(Exclusion::ForLife),
        FOR_LIFE => Err(DecodeError::BadLength { tag }),
        _ => Err(DecodeError::Newer),
    }
}

/// The value of a map of exclusions: the host and the exclusion's value in turn, in key order.
pub(super) fn exclusions_value(exclusions: &BTreeMap<HostId, Exclusion>) -> Vec<u8> {
    let values: Vec<(&str, Vec<u8>)> = exclusions
        .iter()
        .map(|(host, excluded)| (host.as_str(), exclusion(*excluded)))
        .collect();
    map(values
        .iter()
        .map(|(host, value)| (host.as_bytes(), value.as_slice())))
}

/// The exclusions of a row, no host twice.
pub(super) fn exclusions_of(
    tag: u16,
    value: Vec<u8>,
) -> Result<BTreeMap<HostId, Exclusion>, DecodeError> {
    let pairs = pairs_of(tag, &value)?;
    let count = pairs.len();
    let mut by_name = BTreeMap::new();
    for (host, excluded) in pairs {
        by_name.insert(text_of(tag, host.to_vec())?, exclusion_of(tag, excluded)?);
    }
    no_repeats(tag, count, by_name)?
        .into_iter()
        .map(|(host, excluded)| Ok((host_of(tag, host.into_bytes())?, excluded)))
        .collect()
}

/// The row of `mark`.
///
/// # Errors
/// [`JobStoreError::TooLarge`] when a field is over 4 GiB.
pub(crate) fn encode_mark(mark: &MarkRecord) -> Result<Vec<u8>, JobStoreError> {
    let mut row = Writer::default();
    row.put(t::JOB, mark.job.as_str());
    row.put(t::HOST, text(mark.host.as_str()));
    row.put(t::MADE, time(mark.made));
    row.put(t::DEADLINE, time(mark.deadline));
    row.put_opt(t::TAIL_END, mark.tail_end.map(time));
    row.put(t::EXCLUSIONS, exclusions_value(&mark.exclusions));
    row.finish(&mark.carried)
}

/// The mark a row holds.
///
/// # Errors
/// [`DecodeError::Newer`] for a row a later build wrote; another [`DecodeError`] for one that
/// does not parse.
pub(crate) fn decode_mark(row: &[u8]) -> Result<MarkRecord, DecodeError> {
    let mut fields = Fields::of(row)?;
    let job = fields.need(t::JOB, job_id_of)?;
    let host = fields.need(t::HOST, host_of)?;
    let made = fields.need(t::MADE, time_of)?;
    let deadline = fields.need(t::DEADLINE, time_of)?;
    let tail_end = fields.opt(t::TAIL_END, time_of)?;
    let exclusions = fields
        .opt(t::EXCLUSIONS, exclusions_of)?
        .unwrap_or_default();
    Ok(MarkRecord {
        job,
        host,
        made,
        deadline,
        tail_end,
        exclusions,
        carried: fields.carried(),
    })
}

#[cfg(test)]
#[path = "mark_fields_tests.rs"]
mod tests;
