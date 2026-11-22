//! The fields a whole-host job adds to its rows (design sections 5.4 and 6.2), with the same
//! additive rules (B8) as every other field: a row that lacks one reads as its default, and one
//! at its default is not written, so a row that has none of them has the bytes it had before.
//!
//! A job row gains the count of its failed prepares ([`job_whole_tag::FAILED_PREPARES`], a
//! `u32`, written when above zero) and the hosts those prepares keep it off
//! ([`job_whole_tag::PREPARE_BARS`], a map of host to exclusion in the encoding of a mark's
//! exclusions, written when it holds any) and the host of each start a held host refused
//! ([`job_whole_tag::HELD_REFUSALS`], a list of text in order, written when it holds any). An
//! allocation row gains the instant the agent must
//! have reported the host ready ([`alloc_whole_tag::PREPARE_DEADLINE`], a time) and the
//! placement's own wall limit ([`alloc_whole_tag::WALL_LIMIT_SECS`], a `u64`), each written when
//! set. An allocation row also gains which history sized the job
//! ([`alloc_whole_tag::SIZED_FROM`], one byte per [`SizedFrom`]), written when set; a byte this
//! build does not know reads as none, so a later build's value never fails an older reader.

use std::collections::BTreeMap;

use js_state::StoreTime;
use js_types::HostId;

use super::mark_fields::{exclusions_of, exclusions_value};
use super::{DecodeError, Fields, Writer, host_of, texts, time, time_of, u64_of};
use crate::record::{AllocationRecord, Exclusion, JobRecord, SizedFrom};

/// The tags a whole-host job adds to a job row, after the ones of `job_tag`.
pub(crate) mod job_whole_tag {
    /// How many whole-host prepares of the job failed (u32).
    pub const FAILED_PREPARES: u16 = 45;
    /// The hosts those prepares keep the job off (map of text to a kind byte and, but for a
    /// kind of 2, a time).
    pub const PREPARE_BARS: u16 = 46;
    /// The host of each start a held host refused that sent the job back to the queue, in
    /// order (list of text).
    pub const HELD_REFUSALS: u16 = 59;
}

/// The tags a whole-host placement adds to an allocation row, after the ones of `alloc_tag`.
pub(crate) mod alloc_whole_tag {
    /// When the agent must have reported the host ready (time).
    pub const PREPARE_DEADLINE: u16 = 14;
    /// The placement's wall limit in seconds (u64).
    pub const WALL_LIMIT_SECS: u16 = 15;
    /// Which history sized the job (u8: 1 class, 2 fallback, 3 cold, 4 over-cap).
    pub const SIZED_FROM: u16 = 16;
}

/// A `u32`.
pub(super) fn u32_of(tag: u16, value: Vec<u8>) -> Result<u32, DecodeError> {
    value
        .as_slice()
        .try_into()
        .map(u32::from_be_bytes)
        .map_err(|_| DecodeError::BadLength { tag })
}

/// Writes the whole-host fields of `job` that are not at their default.
pub(super) fn put_job(row: &mut Writer, job: &JobRecord) {
    use job_whole_tag as t;
    if job.failed_prepares > 0 {
        row.put(t::FAILED_PREPARES, job.failed_prepares.to_be_bytes());
    }
    if !job.prepare_bars.is_empty() {
        row.put(t::PREPARE_BARS, exclusions_value(&job.prepare_bars));
    }
    if !job.held_refusals.is_empty() {
        let hosts: Vec<String> = job.held_refusals.iter().map(ToString::to_string).collect();
        row.put(t::HELD_REFUSALS, texts(&hosts));
    }
}

/// What [`take_job`] reads of a job row: its failed-prepare count, its bars and its held
/// refusals.
pub(super) type WholeOfJob = (u32, BTreeMap<HostId, Exclusion>, Vec<HostId>);

/// The failed-prepare count, the prepare bars and the held refusals of a job row: zero and none
/// when it has none of them.
pub(super) fn take_job(fields: &mut Fields) -> Result<WholeOfJob, DecodeError> {
    use job_whole_tag as t;
    let failed_prepares = fields.opt(t::FAILED_PREPARES, u32_of)?.unwrap_or(0);
    let prepare_bars = fields
        .opt(t::PREPARE_BARS, exclusions_of)?
        .unwrap_or_default();
    let held_refusals = fields.opt(t::HELD_REFUSALS, hosts_of)?.unwrap_or_default();
    Ok((failed_prepares, prepare_bars, held_refusals))
}

/// A list of hosts, each a host id: a list that does not parse, or an item that is no host id,
/// is refused with the tag.
fn hosts_of(tag: u16, value: Vec<u8>) -> Result<Vec<HostId>, DecodeError> {
    let items = super::items_of(tag, &value)?;
    items
        .into_iter()
        .map(|item| host_of(tag, item.to_vec()))
        .collect()
}

/// Writes the whole-host fields of `allocation` that are set.
pub(super) fn put_allocation(row: &mut Writer, allocation: &AllocationRecord) {
    use alloc_whole_tag as t;
    row.put_opt(t::PREPARE_DEADLINE, allocation.prepare_deadline.map(time));
    row.put_opt(
        t::WALL_LIMIT_SECS,
        allocation
            .wall_limit_secs
            .map(|secs| secs.to_be_bytes().to_vec()),
    );
    row.put_opt(
        t::SIZED_FROM,
        allocation.sized_from.map(|from| vec![sized_code(from)]),
    );
}

/// The byte of `from`.
fn sized_code(from: SizedFrom) -> u8 {
    match from {
        SizedFrom::Class => 1,
        SizedFrom::Fallback => 2,
        SizedFrom::Cold => 3,
        SizedFrom::OverCap => 4,
    }
}

/// The history a sized-from value names; a value of another length or a byte this build does
/// not know is none, never an error.
fn sized_of(_tag: u16, value: Vec<u8>) -> Result<Option<SizedFrom>, DecodeError> {
    Ok(match value.as_slice() {
        [1] => Some(SizedFrom::Class),
        [2] => Some(SizedFrom::Fallback),
        [3] => Some(SizedFrom::Cold),
        [4] => Some(SizedFrom::OverCap),
        _ => None,
    })
}

/// What [`take_alloc`] reads of an allocation row: its prepare deadline, its wall limit and
/// the history that sized it.
pub(super) type WholeOfAllocation = (Option<StoreTime>, Option<u64>, Option<SizedFrom>);

/// The prepare deadline, the wall limit and the sizing history of an allocation row: none when
/// it has none of them.
pub(super) fn take_alloc(fields: &mut Fields) -> Result<WholeOfAllocation, DecodeError> {
    use alloc_whole_tag as t;
    Ok((
        fields.opt(t::PREPARE_DEADLINE, time_of)?,
        fields.opt(t::WALL_LIMIT_SECS, u64_of)?,
        fields.opt(t::SIZED_FROM, sized_of)?.flatten(),
    ))
}

#[cfg(test)]
#[path = "whole_fields_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "held_fields_tests.rs"]
mod held_tests;

#[cfg(test)]
#[path = "sizing_fields_tests.rs"]
mod sizing_tests;
