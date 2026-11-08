//! The fields a job's queue status adds to its row (`docs/design/queue-status.md`, section 5),
//! with the same additive rules (B8) as every other field: a row that lacks one reads as its
//! default, and one at its default is not written, so the row of a job nothing was recorded of
//! has the bytes it had before.
//!
//! The code ([`job_queue_tag::CODE`]) is one byte, the detail text, the time the code last
//! changed a time, the attempts and the dropped events each a `u32`. The events
//! ([`job_queue_tag::EVENTS`]) are a list, each item itself a list of its time, its kind byte,
//! its detail and, when it names one, its host. A code or kind byte this build does not know
//! reads as unspecified rather than refusing the row: the status is derived again on the next
//! pass, and a row refused would hide the job from this build.

use js_state::StoreTime;
use js_types::{JobEventKind, QueueCode};

use super::whole_fields::u32_of;
use super::{
    DecodeError, Fields, Writer, byte_of, host_of, items_of, list, text, text_of, time, time_of,
};
use crate::queue::{JobEvent, QueueTrail};

/// The tags a job's queue status adds to a job row, after the ones of `job_host_tag`.
pub(crate) mod job_queue_tag {
    /// The queue code (u8).
    pub const CODE: u16 = 48;
    /// The code's detail (text).
    pub const DETAIL: u16 = 49;
    /// When the code last changed (time).
    pub const SINCE: u16 = 50;
    /// The placement attempts (u32).
    pub const ATTEMPTS: u16 = 51;
    /// The newest events (list of lists: time, kind byte, detail, and the host when named).
    pub const EVENTS: u16 = 52;
    /// The events dropped to keep the bound (u32).
    pub const DROPPED_EVENTS: u16 = 53;
}

use job_queue_tag as t;

/// The byte of `code`: its number in the job service's `QueueStatusCode`.
fn code_byte(code: QueueCode) -> u8 {
    match code {
        QueueCode::Unspecified => 0,
        QueueCode::WaitingForCapacity => 1,
        QueueCode::WaitingForWholeHost => 2,
        QueueCode::QuotaBackoff => 3,
        QueueCode::FailingOver => 4,
        QueueCode::Placed => 5,
        QueueCode::WaitingForMachine => 6,
    }
}

/// The code of a byte; one this build does not know is unspecified.
fn code_of(tag: u16, value: Vec<u8>) -> Result<QueueCode, DecodeError> {
    Ok(match byte_of(tag, &value)? {
        1 => QueueCode::WaitingForCapacity,
        2 => QueueCode::WaitingForWholeHost,
        3 => QueueCode::QuotaBackoff,
        4 => QueueCode::FailingOver,
        5 => QueueCode::Placed,
        6 => QueueCode::WaitingForMachine,
        _ => QueueCode::Unspecified,
    })
}

/// The byte of `kind`: its number in the job service's `JobEventKind`.
fn kind_byte(kind: JobEventKind) -> u8 {
    match kind {
        JobEventKind::Unspecified => 0,
        JobEventKind::WaitingForCapacity => 1,
        JobEventKind::WaitingForWholeHost => 2,
        JobEventKind::QuotaBackoff => 3,
        JobEventKind::FailingOver => 4,
        JobEventKind::Placed => 5,
        JobEventKind::Requeued => 6,
        JobEventKind::WaitingForMachine => 7,
    }
}

/// The kind of a byte; one this build does not know is unspecified.
fn kind_of(byte: u8) -> JobEventKind {
    match byte {
        1 => JobEventKind::WaitingForCapacity,
        2 => JobEventKind::WaitingForWholeHost,
        3 => JobEventKind::QuotaBackoff,
        4 => JobEventKind::FailingOver,
        5 => JobEventKind::Placed,
        6 => JobEventKind::Requeued,
        7 => JobEventKind::WaitingForMachine,
        _ => JobEventKind::Unspecified,
    }
}

/// The value of one event.
fn event_value(event: &JobEvent) -> Vec<u8> {
    let at = time(event.at);
    let kind = [kind_byte(event.kind)];
    let mut parts: Vec<&[u8]> = vec![&at, &kind, event.detail.as_bytes()];
    if let Some(host) = &event.host {
        parts.push(host.as_str().as_bytes());
    }
    list(parts)
}

/// The event an item holds: its time, kind and detail, and its host when it has four parts.
fn event_of(tag: u16, item: &[u8]) -> Result<JobEvent, DecodeError> {
    let parts = items_of(tag, item)?;
    let (at, kind, detail, host) = match parts.as_slice() {
        [at, kind, detail] => (at, kind, detail, None),
        [at, kind, detail, host] => (at, kind, detail, Some(host)),
        _ => return Err(DecodeError::BadValue { tag }),
    };
    Ok(JobEvent {
        at: time_of(tag, at.to_vec())?,
        kind: kind_of(byte_of(tag, kind)?),
        detail: text_of(tag, detail.to_vec())?,
        host: host.map(|host| host_of(tag, host.to_vec())).transpose()?,
    })
}

/// The events of a list value.
fn events_of(tag: u16, value: Vec<u8>) -> Result<Vec<JobEvent>, DecodeError> {
    items_of(tag, &value)?
        .into_iter()
        .map(|item| event_of(tag, item))
        .collect()
}

/// Writes the queue fields of `trail` that are not at their default.
pub(super) fn put_job(row: &mut Writer, trail: &QueueTrail) {
    if trail.code() != QueueCode::Unspecified {
        row.put(t::CODE, [code_byte(trail.code())]);
    }
    if !trail.detail().is_empty() {
        row.put(t::DETAIL, text(trail.detail()));
    }
    row.put_opt(t::SINCE, trail.since().map(time));
    if trail.attempts() > 0 {
        row.put(t::ATTEMPTS, trail.attempts().to_be_bytes());
    }
    if !trail.events().is_empty() {
        let events: Vec<Vec<u8>> = trail.events().iter().map(event_value).collect();
        row.put(t::EVENTS, list(events.iter().map(Vec::as_slice)));
    }
    if trail.dropped_events() > 0 {
        row.put(t::DROPPED_EVENTS, trail.dropped_events().to_be_bytes());
    }
}

/// The queue trail of a job row: the default when it has none of the fields.
pub(super) fn take_job(fields: &mut Fields) -> Result<QueueTrail, DecodeError> {
    let code = fields.opt(t::CODE, code_of)?.unwrap_or_default();
    let detail = fields.opt(t::DETAIL, text_of)?.unwrap_or_default();
    let since: Option<StoreTime> = fields.opt(t::SINCE, time_of)?;
    let attempts = fields.opt(t::ATTEMPTS, u32_of)?.unwrap_or(0);
    let events = fields.opt(t::EVENTS, events_of)?.unwrap_or_default();
    let dropped = fields.opt(t::DROPPED_EVENTS, u32_of)?.unwrap_or(0);
    Ok(QueueTrail::of_row(
        (code, detail, since),
        attempts,
        events,
        dropped,
    ))
}

#[cfg(test)]
#[path = "queue_fields_tests.rs"]
mod tests;
