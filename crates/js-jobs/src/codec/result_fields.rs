//! The result fields of a job row, tags 31 (the reason) to 41 (the stream lengths). They are
//! written only when the job has a result, and the reason is what says it has one: a row
//! without it, as every row written before results were stored, reads with no result. A result
//! without tag 41, as every result written before the lengths were stored, reads with none; a
//! build from before tag 41 keeps it as a field it does not know and writes it back unchanged.

use js_state::StoreTime;

use super::{
    DecodeError, Fields, Writer, bool_of, byte_of, i32_of, job_tag as t, text, text_of, time,
    time_of, u64_of,
};
use crate::result::{JobResult, JobTiming, LogLengths, ResourceUsage, ResultReason};

/// The bytes of a time value: whole seconds (`u64`) and nanoseconds (`u32`).
const TIME_LEN: usize = 12;

/// The bytes of a lengths value: four `u64`.
const LOGS_LEN: usize = 32;

/// Puts the fields of `result`, when there is one, in `row`.
pub(super) fn put_result(row: &mut Writer, result: Option<&JobResult>) {
    let Some(result) = result else {
        return;
    };
    row.put(t::RESULT_REASON, [reason_code(result.reason)]);
    row.put_opt(
        t::RESULT_EXIT_CODE,
        result.exit_code.map(|code| code.to_be_bytes().to_vec()),
    );
    row.put(t::RESULT_STDOUT, text(&result.stdout_tail));
    row.put(t::RESULT_STDERR, text(&result.stderr_tail));
    row.put(t::RESULT_TRUNCATED, [u8::from(result.truncated)]);
    let timing = result.timing;
    row.put(t::RESULT_QUEUED_AT, time(timing.queued_at));
    row.put_opt(t::RESULT_PLACED_AT, timing.placed_at.map(time));
    row.put_opt(t::RESULT_STARTED_AT, timing.started_at.map(time));
    row.put(t::RESULT_ENDED_AT, time(timing.ended_at));
    row.put_opt(t::RESULT_USAGE, result.usage.map(usage));
    row.put_opt(t::RESULT_LOGS, result.logs.map(lengths));
}

/// The result the row holds: `None` when it holds no reason.
pub(super) fn take_result(fields: &mut Fields) -> Result<Option<JobResult>, DecodeError> {
    let Some(reason) = fields.opt(t::RESULT_REASON, reason_of)? else {
        return Ok(None);
    };
    Ok(Some(JobResult {
        exit_code: fields.opt(t::RESULT_EXIT_CODE, i32_of)?,
        reason,
        stdout_tail: fields.opt(t::RESULT_STDOUT, text_of)?.unwrap_or_default(),
        stderr_tail: fields.opt(t::RESULT_STDERR, text_of)?.unwrap_or_default(),
        truncated: fields.opt(t::RESULT_TRUNCATED, bool_of)?.unwrap_or(false),
        timing: JobTiming {
            queued_at: fields.need(t::RESULT_QUEUED_AT, time_of)?,
            placed_at: fields.opt(t::RESULT_PLACED_AT, time_of)?,
            started_at: fields.opt(t::RESULT_STARTED_AT, time_of)?,
            ended_at: fields.need(t::RESULT_ENDED_AT, time_of)?,
        },
        usage: fields.opt(t::RESULT_USAGE, usage_of)?,
        logs: fields.opt(t::RESULT_LOGS, lengths_of)?,
    }))
}

/// A lengths value: the bytes of standard output and of standard error, then the bytes each
/// dropped, each a `u64`.
fn lengths(logs: LogLengths) -> Vec<u8> {
    [
        logs.stdout_bytes,
        logs.stderr_bytes,
        logs.stdout_dropped,
        logs.stderr_dropped,
    ]
    .iter()
    .flat_map(|value| value.to_be_bytes())
    .collect()
}

fn lengths_of(tag: u16, value: Vec<u8>) -> Result<LogLengths, DecodeError> {
    let words: [u8; LOGS_LEN] = value
        .try_into()
        .map_err(|_| DecodeError::BadLength { tag })?;
    let word = |n: usize| {
        let mut bytes = [0; 8];
        bytes.copy_from_slice(&words[n * 8..n * 8 + 8]);
        u64::from_be_bytes(bytes)
    };
    Ok(LogLengths {
        stdout_bytes: word(0),
        stderr_bytes: word(1),
        stdout_dropped: word(2),
        stderr_dropped: word(3),
    })
}

/// A usage value: the CPU time as a time value, then the peak memory as a `u64`.
fn usage(usage: ResourceUsage) -> Vec<u8> {
    let mut out = time(StoreTime::from_duration(usage.cpu_time));
    out.extend_from_slice(&usage.peak_memory_bytes.to_be_bytes());
    out
}

fn usage_of(tag: u16, value: Vec<u8>) -> Result<ResourceUsage, DecodeError> {
    let (cpu, peak) = value
        .split_at_checked(TIME_LEN)
        .ok_or(DecodeError::BadLength { tag })?;
    Ok(ResourceUsage {
        cpu_time: time_of(tag, cpu.to_vec())?.as_duration(),
        peak_memory_bytes: u64_of(tag, peak.to_vec())?,
    })
}

fn reason_code(reason: ResultReason) -> u8 {
    match reason {
        ResultReason::Exit => 0,
        ResultReason::WallLimit => 1,
        ResultReason::Memory => 2,
        ResultReason::Cancelled => 3,
    }
}

fn reason_of(tag: u16, value: Vec<u8>) -> Result<ResultReason, DecodeError> {
    match byte_of(tag, &value)? {
        0 => Ok(ResultReason::Exit),
        1 => Ok(ResultReason::WallLimit),
        2 => Ok(ResultReason::Memory),
        3 => Ok(ResultReason::Cancelled),
        _ => Err(DecodeError::Newer),
    }
}
