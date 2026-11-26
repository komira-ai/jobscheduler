//! The bytes of a job's completion callback (`docs/design/job-retry-and-completion-callback.md`,
//! sections 5.2 and 5.5), with the same additive rules (B8) as every other field: the field
//! [`job_callback_tag::CALLBACK`] of a job row, and the row `callback/<job id>`
//! ([`CallbackRecord`]), whose tags are those of [`callback_tag`].
//!
//! The job row's field is a list of three items: the URL (text), the SHA-256 of the signing key
//! (32 bytes) and the deadline in seconds (four bytes, or empty when the job names none). It is
//! written only for a job that asks for a callback, and it never holds the key: a record whose
//! callback holds its key is written with the key's digest in its place. A build from before the
//! field carries it into its next write of the row ([`super::Carried`]).
//!
//! The callback row holds the key itself until the delivery ends, and the delivery's state as a
//! byte (0 armed, 1 pending, 2 delivered, 3 expired, 4 abandoned). A state or a failure class
//! this build does not know is [`DecodeError::Newer`], as an enum value of a job row is.

use js_state::StoreTime;
use js_types::{FailureClass, SigningKey};

use super::{DecodeError, Fields, Writer, items_of, job_id_of, list, text, text_of, time, time_of};
use crate::callback::{CallbackKey, CallbackRecord, CallbackState, JobCallback, KeyDigest};
use crate::error::JobStoreError;
use crate::ids::InstanceId;

/// The tag the callback adds to a job row, after the history key's 66 and the 67 the job's
/// retry policy takes.
pub(crate) mod job_callback_tag {
    /// The job's callback (a list: URL, key digest, deadline).
    pub const CALLBACK: u16 = 68;
}

/// The tags of a callback row.
pub(crate) mod callback_tag {
    /// The job (text).
    pub const JOB: u16 = 1;
    /// The URL (text).
    pub const URL: u16 = 2;
    /// The signing key (32 to 64 bytes); absent once the delivery ended.
    pub const KEY: u16 = 3;
    /// The `webhook-id` (text).
    pub const WEBHOOK_ID: u16 = 4;
    /// The deadline the job asked for, in seconds (u32).
    pub const DEADLINE_SECONDS: u16 = 5;
    /// The state (u8).
    pub const STATE: u16 = 6;
    /// How many calls were tried (u32).
    pub const TRIES: u16 = 7;
    /// When the next call is due (time).
    pub const NEXT_TRY_AT: u16 = 8;
    /// Until when the server keeps trying (time).
    pub const DEADLINE_AT: u16 = 9;
    /// When the last call was tried (time).
    pub const LAST_TRY_AT: u16 = 10;
    /// The class of the last failed try (u8: 0 refused address, 1 failed).
    pub const LAST_FAILURE: u16 = 11;
    /// When the receiver answered 2xx (time).
    pub const DELIVERED_AT: u16 = 12;
    /// The instance whose claim holds the row (text).
    pub const CLAIMED_BY: u16 = 13;
    /// Until when that claim holds it (time).
    pub const CLAIM_UNTIL: u16 = 14;
}

use callback_tag as t;

/// Puts `callback` in a job row, with its key's digest in place of the key; nothing for a job
/// without one.
pub(super) fn put_spec(row: &mut Writer, callback: Option<&JobCallback>) {
    let value = callback.map(|callback| {
        let digest = callback.key.digest();
        let deadline = callback
            .deadline_seconds
            .map(u32::to_be_bytes)
            .map(Vec::from)
            .unwrap_or_default();
        let items: [&[u8]; 3] = [callback.url.as_bytes(), digest.bytes(), &deadline];
        list(items)
    });
    row.put_opt(job_callback_tag::CALLBACK, value);
}

/// The callback of a job row: none when the row lacks the field.
pub(super) fn take_spec(fields: &mut Fields) -> Result<Option<JobCallback>, DecodeError> {
    fields.opt(job_callback_tag::CALLBACK, callback_of)
}

/// The callback a job row's field holds.
fn callback_of(tag: u16, value: Vec<u8>) -> Result<JobCallback, DecodeError> {
    let items = items_of(tag, &value)?;
    let [url, digest, deadline] = items.as_slice() else {
        return Err(DecodeError::BadValue { tag });
    };
    let digest: [u8; 32] = (*digest)
        .try_into()
        .map_err(|_| DecodeError::BadLength { tag })?;
    let deadline_seconds = match *deadline {
        [] => None,
        bytes => Some(u32_of(tag, bytes)?),
    };
    Ok(JobCallback {
        url: text_of(tag, url.to_vec())?,
        key: CallbackKey::Digest(KeyDigest(digest)),
        deadline_seconds,
    })
}

/// A `u32` value.
fn u32_of(tag: u16, value: &[u8]) -> Result<u32, DecodeError> {
    value
        .try_into()
        .map(u32::from_be_bytes)
        .map_err(|_| DecodeError::BadLength { tag })
}

/// The byte of `state`.
fn state_code(state: CallbackState) -> u8 {
    match state {
        CallbackState::Armed => 0,
        CallbackState::Pending => 1,
        CallbackState::Delivered => 2,
        CallbackState::Expired => 3,
        CallbackState::Abandoned => 4,
    }
}

/// The state a byte holds; one this build does not know is a later build's.
fn state_of(tag: u16, value: Vec<u8>) -> Result<CallbackState, DecodeError> {
    match value.as_slice() {
        [0] => Ok(CallbackState::Armed),
        [1] => Ok(CallbackState::Pending),
        [2] => Ok(CallbackState::Delivered),
        [3] => Ok(CallbackState::Expired),
        [4] => Ok(CallbackState::Abandoned),
        [_] => Err(DecodeError::Newer),
        _ => Err(DecodeError::BadLength { tag }),
    }
}

/// The byte of `class`.
fn class_code(class: FailureClass) -> u8 {
    match class {
        FailureClass::RefusedAddress => 0,
        FailureClass::Failed => 1,
    }
}

/// The failure class a byte holds; one this build does not know is a later build's.
fn class_of(tag: u16, value: Vec<u8>) -> Result<FailureClass, DecodeError> {
    match value.as_slice() {
        [0] => Ok(FailureClass::RefusedAddress),
        [1] => Ok(FailureClass::Failed),
        [_] => Err(DecodeError::Newer),
        _ => Err(DecodeError::BadLength { tag }),
    }
}

/// The signing key a value holds.
fn key_of(tag: u16, value: Vec<u8>) -> Result<SigningKey, DecodeError> {
    SigningKey::new(value).map_err(|_| DecodeError::BadValue { tag })
}

/// The row of `callback`.
///
/// # Errors
/// [`JobStoreError::TooLarge`] when a field is over 4 GiB.
pub(crate) fn encode_callback(callback: &CallbackRecord) -> Result<Vec<u8>, JobStoreError> {
    let mut row = Writer::default();
    let at = |at: Option<StoreTime>| at.map(time);
    row.put(t::JOB, callback.job.as_str());
    row.put(t::URL, text(&callback.url));
    row.put_opt(
        t::KEY,
        callback.key.as_ref().map(|key| key.as_bytes().to_vec()),
    );
    row.put(t::WEBHOOK_ID, text(&callback.webhook_id));
    let deadline = callback
        .deadline_seconds
        .map(|secs| secs.to_be_bytes().to_vec());
    row.put_opt(t::DEADLINE_SECONDS, deadline);
    row.put(t::STATE, [state_code(callback.state)]);
    row.put(t::TRIES, callback.tries.to_be_bytes());
    row.put_opt(t::NEXT_TRY_AT, at(callback.next_try_at));
    row.put_opt(t::DEADLINE_AT, at(callback.deadline_at));
    row.put_opt(t::LAST_TRY_AT, at(callback.last_try_at));
    let failure = callback.last_failure.map(|class| vec![class_code(class)]);
    row.put_opt(t::LAST_FAILURE, failure);
    row.put_opt(t::DELIVERED_AT, at(callback.delivered_at));
    let claimed_by = callback.claimed_by.as_ref().map(|by| text(by.as_str()));
    row.put_opt(t::CLAIMED_BY, claimed_by);
    row.put_opt(t::CLAIM_UNTIL, at(callback.claim_until));
    row.finish(&callback.carried)
}

/// The callback a row holds.
///
/// # Errors
/// [`DecodeError::Newer`] for a row a later build wrote; another [`DecodeError`] for a bad one.
pub(crate) fn decode_callback(row: &[u8]) -> Result<CallbackRecord, DecodeError> {
    let mut fields = Fields::of(row)?;
    let tries = |tag, value: Vec<u8>| u32_of(tag, &value);
    let deadline_seconds = |tag, value: Vec<u8>| u32_of(tag, &value);
    let claimed_by = |tag, value| text_of(tag, value).map(InstanceId::new);
    Ok(CallbackRecord {
        job: fields.need(t::JOB, job_id_of)?,
        url: fields.need(t::URL, text_of)?,
        key: fields.opt(t::KEY, key_of)?,
        webhook_id: fields.need(t::WEBHOOK_ID, text_of)?,
        deadline_seconds: fields.opt(t::DEADLINE_SECONDS, deadline_seconds)?,
        state: fields.need(t::STATE, state_of)?,
        tries: fields.opt(t::TRIES, tries)?.unwrap_or(0),
        next_try_at: fields.opt(t::NEXT_TRY_AT, time_of)?,
        deadline_at: fields.opt(t::DEADLINE_AT, time_of)?,
        last_try_at: fields.opt(t::LAST_TRY_AT, time_of)?,
        last_failure: fields.opt(t::LAST_FAILURE, class_of)?,
        delivered_at: fields.opt(t::DELIVERED_AT, time_of)?,
        claimed_by: fields.opt(t::CLAIMED_BY, claimed_by)?,
        claim_until: fields.opt(t::CLAIM_UNTIL, time_of)?,
        carried: fields.carried(),
    })
}

#[cfg(test)]
#[path = "callback_fields_tests.rs"]
mod tests;
