//! The bytes of a dispatch message row (`dispatch/<host>/<seq>`, [`DispatchMessage`]): the same
//! format byte and tag-length-value fields as a job or allocation row, with the tags of
//! [`dispatch_tag`], and the same additive rules (B8): a tag this build does not know is carried,
//! an optional tag the row lacks (the start body, the claim) reads as `None`, and a format byte or a kind this build does
//! not know is [`DecodeError::Newer`].
//!
//! The start body is the agent's own [`JobStartSpec`] as the agent protocol writes it (JSON),
//! whose fields change only additively too: a field a later build adds is ignored here, as an
//! agent ignores it.

use js_agent_wire::JobStartSpec;

use super::{
    DecodeError, Fields, Writer, allocation_id_of, byte_of, host_of, job_id_of, text, text_of,
    time, time_of,
};
use crate::error::JobStoreError;
use crate::record::{DispatchKind, DispatchMessage};

/// The tags of a dispatch message row.
pub(crate) mod dispatch_tag {
    /// Start or stop (u8: 1 a start, 2 a stop).
    pub const KIND: u16 = 1;
    /// The job (text).
    pub const JOB: u16 = 2;
    /// The allocation id (16 bytes).
    pub const ALLOCATION_ID: u16 = 3;
    /// The host (text).
    pub const HOST: u16 = 4;
    /// When it was queued (time).
    pub const QUEUED_AT: u16 = 5;
    /// The start body (JSON of the agent protocol's job start).
    pub const START: u16 = 6;
    /// The server that last handed it out (text).
    pub const CLAIMED_BY: u16 = 7;
    /// Until when the last hand-out holds it (time).
    pub const CLAIM_UNTIL: u16 = 8;
}

use dispatch_tag as t;

/// The kind byte of a start.
const START: u8 = 1;
/// The kind byte of a stop.
const STOP: u8 = 2;

/// The kind a byte holds: a byte this build does not know is a later build's.
fn kind_of(tag: u16, value: Vec<u8>) -> Result<DispatchKind, DecodeError> {
    match byte_of(tag, &value)? {
        START => Ok(DispatchKind::Start),
        STOP => Ok(DispatchKind::Stop),
        _ => Err(DecodeError::Newer),
    }
}

/// The start body a value holds.
fn start_of(tag: u16, value: Vec<u8>) -> Result<JobStartSpec, DecodeError> {
    serde_json::from_slice(&value).map_err(|_| DecodeError::BadValue { tag })
}

/// The row of `message`.
///
/// # Errors
/// [`JobStoreError::TooLarge`] when a field is over 4 GiB.
pub(crate) fn encode_dispatch(message: &DispatchMessage) -> Result<Vec<u8>, JobStoreError> {
    let mut row = Writer::default();
    let kind = match message.kind {
        DispatchKind::Start => START,
        DispatchKind::Stop => STOP,
    };
    row.put(t::KIND, [kind]);
    row.put(t::JOB, message.job.as_str());
    row.put(t::ALLOCATION_ID, message.allocation_id.bits());
    row.put(t::HOST, text(message.host.as_str()));
    row.put(t::QUEUED_AT, time(message.queued_at));
    // A job start is plain data whose every map has text keys, which JSON always writes.
    let start = message.start.as_ref();
    row.put_opt(
        t::START,
        start.and_then(|spec| serde_json::to_vec(spec).ok()),
    );
    row.put_opt(t::CLAIMED_BY, message.claimed_by.as_deref().map(text));
    row.put_opt(t::CLAIM_UNTIL, message.claim_until.map(time));
    row.finish(&message.carried)
}

/// The dispatch message a row holds.
///
/// # Errors
/// [`DecodeError::Newer`] for a row a later build wrote; another [`DecodeError`] for one that
/// does not parse.
pub(crate) fn decode_dispatch(row: &[u8]) -> Result<DispatchMessage, DecodeError> {
    let mut fields = Fields::of(row)?;
    let kind = fields.need(t::KIND, kind_of)?;
    let job = fields.need(t::JOB, job_id_of)?;
    let allocation_id = fields.need(t::ALLOCATION_ID, allocation_id_of)?;
    let host = fields.need(t::HOST, host_of)?;
    let queued_at = fields.need(t::QUEUED_AT, time_of)?;
    let start = fields.opt(t::START, start_of)?;
    let claimed_by = fields.opt(t::CLAIMED_BY, text_of)?;
    let claim_until = fields.opt(t::CLAIM_UNTIL, time_of)?;
    Ok(DispatchMessage {
        kind,
        job,
        allocation_id,
        host,
        queued_at,
        start,
        claimed_by,
        claim_until,
        carried: fields.carried(),
    })
}

#[cfg(test)]
#[path = "dispatch_fields_tests.rs"]
mod tests;
