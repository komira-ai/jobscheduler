//! The fields of a job's retry policy and its attempts
//! (`docs/design/job-retry-and-completion-callback.md`, sections 3.1 and 3.7), and the row of
//! one attempt, with the same additive rules (B8) as every other field.
//!
//! A job row gains its retry policy ([`job_attempt_tag::RETRY`], in the spec), the number of
//! its current attempt ([`job_attempt_tag::ATTEMPT`], a `u32`, written when it is not the
//! first), its ended attempts ([`job_attempt_tag::ATTEMPT_HISTORY`], a list, written when it
//! holds any), when its next attempt may be placed ([`job_attempt_tag::RETRY_NOT_BEFORE`], a
//! time), how many ended attempts the history dropped ([`job_attempt_tag::DROPPED_ATTEMPTS`],
//! a `u32`, written when above zero) and every host that held an ended attempt
//! ([`job_attempt_tag::ATTEMPT_HOSTS`], a list of text in order, written when it holds any).
//! A row with none of them, which is every row of a job never placed and every row a build
//! before them wrote, reads as the first attempt with nothing ended and no policy, and has the
//! bytes it had before. Tag 67 is the callback's (the same design, section 5.2), which this
//! build does not write.
//!
//! The policy is a list of three items: the most attempts (a `u32`), then the initial and the
//! longest wait, each a time's bytes (the seconds and the nanoseconds of a duration) or empty
//! when the policy leaves it to its default. Any other shape is [`DecodeError::BadValue`]: a
//! later field of the policy is a job tag of its own, not a fourth item.
//!
//! Each ended attempt is an item of the history's list whose bytes are a row of their own: a
//! format byte and tag-length-value fields ([`attempt_tag`]). A tag of an attempt this build
//! does not know is kept with the attempt and written back, as a job row's is, so a build one
//! release back rewrites a later build's history without losing its fields.
//!
//! The row of one attempt, `attempt/<job id>/<n>`, is a format byte and the fields of
//! [`attempt_row_tag`]: the job and the attempt's number, which its key repeats and a read
//! checks, its host, and when it was placed and ended.

use std::collections::BTreeSet;
use std::time::Duration;

use js_state::StoreTime;
use js_types::{HostId, RetryPolicy};

use super::{
    Carried, DecodeError, Fields, Writer, bool_of, byte_of, host_of, i32_of, items_of, job_id_of,
    list, result_fields, state_code, state_of, text, text_of, texts, time, time_of,
    whole_fields::u32_of,
};
use crate::attempt::{Attempt, AttemptOutcome, AttemptRecord, AttemptRow, AttemptTrail};
use crate::error::JobStoreError;
use crate::ids::JobId;
use crate::record::{JobState, StartFailure};

/// The tags a job's retry policy and attempts add to a job row, after the ones of `job_tag`
/// and the other `*_fields` modules.
pub(crate) mod job_attempt_tag {
    /// The number of the job's current attempt (u32), when it is not the first.
    pub const ATTEMPT: u16 = 68;
    /// The attempts that ended, oldest first (a list of attempt rows).
    pub const ATTEMPT_HISTORY: u16 = 69;
    /// When the job's next attempt may be placed (time).
    pub const RETRY_NOT_BEFORE: u16 = 70;
    /// How many ended attempts the history dropped (u32).
    pub const DROPPED_ATTEMPTS: u16 = 71;
    /// Every host that held an ended attempt, in order (list of text).
    pub const ATTEMPT_HOSTS: u16 = 72;
    /// The retry policy (a list of three items), part of the spec.
    pub const RETRY: u16 = 73;
}

/// The tags of one ended attempt, an item of [`job_attempt_tag::ATTEMPT_HISTORY`].
pub(crate) mod attempt_tag {
    /// Its number (u32).
    pub const NUMBER: u16 = 1;
    /// Its host (text).
    pub const HOST: u16 = 2;
    /// How it ended (u8: 0 succeeded, 1 failed, 2 host lost, 3 start failed, 4 cancelled).
    pub const OUTCOME: u16 = 3;
    /// When it became placeable (time).
    pub const QUEUED_AT: u16 = 4;
    /// When it was placed (time).
    pub const PLACED_AT: u16 = 5;
    /// When it began running (time).
    pub const STARTED_AT: u16 = 6;
    /// When it ended (time).
    pub const ENDED_AT: u16 = 7;
    /// Its exit code (i32).
    pub const EXIT_CODE: u16 = 8;
    /// Why it ended, as a result says (u8, the result's reason codes).
    pub const REASON: u16 = 9;
    /// Why it failed to start (u8, the job state code of that start failure).
    pub const START_FAILURE: u16 = 10;
    /// The agent's code for a start it refused (text).
    pub const START_FAILURE_DETAIL: u16 = 11;
    /// Whether another attempt was queued after it (u8, 0 or 1).
    pub const RETRIED: u16 = 12;
}

/// The tags of the row of one attempt, `attempt/<job id>/<n>`.
pub(crate) mod attempt_row_tag {
    /// The job (text).
    pub const JOB: u16 = 1;
    /// The attempt's number (u32).
    pub const NUMBER: u16 = 2;
    /// Its host (text).
    pub const HOST: u16 = 3;
    /// When it was placed (time).
    pub const PLACED_AT: u16 = 4;
    /// When it ended (time).
    pub const ENDED_AT: u16 = 5;
}

/// The bytes of a duration, as a time's.
fn duration(wait: Duration) -> Vec<u8> {
    time(StoreTime::from_duration(wait))
}

/// Writes `retry`, when the job has a policy.
pub(super) fn put_retry(row: &mut Writer, retry: Option<&RetryPolicy>) {
    row.put_opt(
        job_attempt_tag::RETRY,
        retry.map(|policy| {
            let initial = policy.initial_backoff.map(duration).unwrap_or_default();
            let longest = policy.max_backoff.map(duration).unwrap_or_default();
            let most = policy.max_attempts.to_be_bytes();
            list([&most[..], &initial, &longest])
        }),
    );
}

/// The retry policy of a row: none when the row lacks the tag.
pub(super) fn take_retry(fields: &mut Fields) -> Result<Option<RetryPolicy>, DecodeError> {
    fields.opt(job_attempt_tag::RETRY, retry_of)
}

/// A retry policy's value: exactly three items, as the module documents.
fn retry_of(tag: u16, value: Vec<u8>) -> Result<RetryPolicy, DecodeError> {
    let [most, initial, longest] = items_of(tag, &value)?[..] else {
        return Err(DecodeError::BadValue { tag });
    };
    let wait = |item: &[u8]| -> Result<Option<Duration>, DecodeError> {
        let given = (!item.is_empty()).then(|| time_of(tag, item.to_vec()));
        Ok(given.transpose()?.map(StoreTime::as_duration))
    };
    Ok(RetryPolicy {
        max_attempts: u32_of(tag, most.to_vec())?,
        initial_backoff: wait(initial)?,
        max_backoff: wait(longest)?,
    })
}

/// Writes the fields of `trail` that are not at their default.
pub(super) fn put_job(row: &mut Writer, trail: &AttemptTrail) -> Result<(), JobStoreError> {
    use job_attempt_tag as t;
    if trail.number() != 1 {
        row.put(t::ATTEMPT, trail.number().to_be_bytes());
    }
    if !trail.history().is_empty() {
        let items = trail
            .history()
            .iter()
            .map(attempt_value)
            .collect::<Result<Vec<_>, _>>()?;
        row.put(t::ATTEMPT_HISTORY, list(items.iter().map(Vec::as_slice)));
    }
    row.put_opt(t::RETRY_NOT_BEFORE, trail.retry_not_before().map(time));
    if trail.dropped() > 0 {
        row.put(t::DROPPED_ATTEMPTS, trail.dropped().to_be_bytes());
    }
    if !trail.hosts().is_empty() {
        let hosts: Vec<String> = trail.hosts().iter().map(ToString::to_string).collect();
        row.put(t::ATTEMPT_HOSTS, texts(&hosts));
    }
    Ok(())
}

/// The attempts of a job row: the first attempt with nothing ended when it has none of them.
pub(super) fn take_job(fields: &mut Fields) -> Result<AttemptTrail, DecodeError> {
    use job_attempt_tag as t;
    let number = fields.opt(t::ATTEMPT, number_of)?.unwrap_or(1);
    let history = fields.opt(t::ATTEMPT_HISTORY, history_of)?;
    let retry_not_before = fields.opt(t::RETRY_NOT_BEFORE, time_of)?;
    let dropped = fields.opt(t::DROPPED_ATTEMPTS, u32_of)?.unwrap_or(0);
    let hosts = fields.opt(t::ATTEMPT_HOSTS, hosts_of)?.unwrap_or_default();
    Ok(AttemptTrail::from_parts(
        number,
        history.unwrap_or_default(),
        dropped,
        retry_not_before,
        hosts,
    ))
}

/// An attempt's number: a `u32` from 1.
fn number_of(tag: u16, value: Vec<u8>) -> Result<u32, DecodeError> {
    let number = u32_of(tag, value)?;
    (number > 0)
        .then_some(number)
        .ok_or(DecodeError::BadValue { tag })
}

/// The hosts of [`job_attempt_tag::ATTEMPT_HOSTS`]: each a host id, none twice.
fn hosts_of(tag: u16, value: Vec<u8>) -> Result<BTreeSet<HostId>, DecodeError> {
    let items = items_of(tag, &value)?;
    let count = items.len();
    let hosts = items
        .into_iter()
        .map(|item| host_of(tag, item.to_vec()))
        .collect::<Result<BTreeSet<_>, _>>()?;
    (hosts.len() == count)
        .then_some(hosts)
        .ok_or(DecodeError::BadValue { tag })
}

/// The byte of `outcome`.
fn outcome_code(outcome: AttemptOutcome) -> u8 {
    match outcome {
        AttemptOutcome::Succeeded => 0,
        AttemptOutcome::Failed => 1,
        AttemptOutcome::HostLost => 2,
        AttemptOutcome::StartFailed => 3,
        AttemptOutcome::Cancelled => 4,
    }
}

/// The outcome a byte names; a byte this build does not know is a later build's.
fn outcome_of(tag: u16, value: Vec<u8>) -> Result<AttemptOutcome, DecodeError> {
    match byte_of(tag, &value)? {
        0 => Ok(AttemptOutcome::Succeeded),
        1 => Ok(AttemptOutcome::Failed),
        2 => Ok(AttemptOutcome::HostLost),
        3 => Ok(AttemptOutcome::StartFailed),
        4 => Ok(AttemptOutcome::Cancelled),
        _ => Err(DecodeError::Newer),
    }
}

/// The start failure a byte names: the job state code of failing to start so.
fn start_failure_of(tag: u16, value: Vec<u8>) -> Result<StartFailure, DecodeError> {
    match state_of(tag, value)? {
        JobState::FailedToStart(failure) => Ok(failure),
        _ => Err(DecodeError::BadValue { tag }),
    }
}

/// The bytes of one ended attempt: a row of its own, its unknown fields kept.
fn attempt_value(attempt: &Attempt) -> Result<Vec<u8>, JobStoreError> {
    use attempt_tag as t;
    let mut row = Writer::default();
    let end = &attempt.end;
    row.put(t::NUMBER, attempt.number.to_be_bytes());
    row.put(t::HOST, text(attempt.host.as_str()));
    row.put(t::OUTCOME, [outcome_code(end.outcome)]);
    row.put(t::QUEUED_AT, time(attempt.queued_at));
    row.put(t::PLACED_AT, time(attempt.placed_at));
    row.put_opt(t::STARTED_AT, attempt.started_at.map(time));
    row.put(t::ENDED_AT, time(attempt.ended_at));
    row.put_opt(
        t::EXIT_CODE,
        end.exit_code.map(|code| code.to_be_bytes().to_vec()),
    );
    row.put_opt(
        t::REASON,
        end.reason
            .map(|reason| vec![result_fields::reason_code(reason)]),
    );
    row.put_opt(
        t::START_FAILURE,
        end.start_failure
            .map(|failure| vec![state_code(JobState::FailedToStart(failure))]),
    );
    row.put_opt(
        t::START_FAILURE_DETAIL,
        end.start_failure_detail.as_deref().map(text),
    );
    row.put(t::RETRIED, [u8::from(attempt.retried)]);
    row.finish(&attempt.carried)
}

/// The ended attempts of [`job_attempt_tag::ATTEMPT_HISTORY`].
fn history_of(tag: u16, value: Vec<u8>) -> Result<Vec<Attempt>, DecodeError> {
    items_of(tag, &value)?.into_iter().map(attempt_of).collect()
}

/// One ended attempt, from its bytes.
fn attempt_of(item: &[u8]) -> Result<Attempt, DecodeError> {
    use attempt_tag as t;
    let mut fields = Fields::of(item)?;
    let end = AttemptRecord {
        outcome: fields.need(t::OUTCOME, outcome_of)?,
        exit_code: fields.opt(t::EXIT_CODE, i32_of)?,
        reason: fields.opt(t::REASON, result_fields::reason_of)?,
        start_failure: fields.opt(t::START_FAILURE, start_failure_of)?,
        start_failure_detail: fields.opt(t::START_FAILURE_DETAIL, text_of)?,
    };
    Ok(Attempt {
        number: fields.need(t::NUMBER, number_of)?,
        host: fields.need(t::HOST, host_of)?,
        queued_at: fields.need(t::QUEUED_AT, time_of)?,
        placed_at: fields.need(t::PLACED_AT, time_of)?,
        started_at: fields.opt(t::STARTED_AT, time_of)?,
        ended_at: fields.need(t::ENDED_AT, time_of)?,
        end,
        retried: fields.opt(t::RETRIED, bool_of)?.unwrap_or(false),
        carried: fields.carried(),
    })
}

/// The row of the ended attempt `attempt` of the job `job`.
///
/// # Errors
/// [`JobStoreError::TooLarge`] when a field is over 4 GiB.
pub(crate) fn encode_attempt_row(job: &JobId, attempt: &Attempt) -> Result<Vec<u8>, JobStoreError> {
    use attempt_row_tag as t;
    let mut row = Writer::default();
    row.put(t::JOB, job.as_str());
    row.put(t::NUMBER, attempt.number.to_be_bytes());
    row.put(t::HOST, text(attempt.host.as_str()));
    row.put(t::PLACED_AT, time(attempt.placed_at));
    row.put(t::ENDED_AT, time(attempt.ended_at));
    row.finish(&Carried::default())
}

/// The job, the attempt's number and the attempt a row of one attempt holds.
///
/// # Errors
/// [`DecodeError::Newer`] for a row a later build wrote; another [`DecodeError`] for a bad
/// one.
pub(crate) fn decode_attempt_row(row: &[u8]) -> Result<(JobId, u32, AttemptRow), DecodeError> {
    use attempt_row_tag as t;
    let mut fields = Fields::of(row)?;
    let job = fields.need(t::JOB, job_id_of)?;
    let number = fields.need(t::NUMBER, number_of)?;
    let attempt = AttemptRow {
        host: fields.need(t::HOST, host_of)?,
        placed_at: fields.need(t::PLACED_AT, time_of)?,
        ended_at: fields.need(t::ENDED_AT, time_of)?,
    };
    Ok((job, number, attempt))
}

#[cfg(test)]
#[path = "attempt_fields_tests.rs"]
mod tests;
