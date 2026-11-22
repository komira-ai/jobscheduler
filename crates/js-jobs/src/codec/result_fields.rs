//! The result fields of a job row, tags 31 (the reason) to 40 (the usage), 43 (the stream
//! lengths), 58 (the code of an input that failed, `rootfs_fields`) and 60 to 62 (the CPU
//! throttle counters, the CPU pressure and the CPU peak of the usage). They are written only when the job has
//! a result, and the reason is what says it has one: a row without it, as every row written before results were stored, reads with no
//! result. A result without tag 43, as every result written before the lengths were stored,
//! reads with none; a build from before tag 43 keeps it as a field it does not know and writes
//! it back unchanged. Tags 60 to 62 are written only for a usage that has the value, and read
//! only beside tag 40: a row with one of them and no tag 40 reads with no usage. A usage
//! without them, as every one written before them, reads with `None`; a build from before
//! them keeps them as fields it does not know and writes them back unchanged.

use js_state::StoreTime;

use self::job_result_tag::{RESULT_CPU_PEAK, RESULT_CPU_PRESSURE_SOME, RESULT_CPU_THROTTLE};
use super::rootfs_fields::job_rootfs_tag::RESULT_INPUT_FAILURE as INPUT_FAILURE;
use super::{
    DecodeError, Fields, Writer, bool_of, byte_of, i32_of, job_tag as t, text, text_of, time,
    time_of, u64_of,
};
use crate::result::{
    CpuPeak, CpuThrottle, JobResult, JobTiming, LogLengths, ResourceUsage, ResultReason,
};

/// The bytes of a time value: whole seconds (`u64`) and nanoseconds (`u32`).
const TIME_LEN: usize = 12;

/// The bytes of a lengths value: four `u64`.
const LOGS_LEN: usize = 32;

/// The job-row tags of the usage's CPU counters, beside the result tags of
/// [`crate::codec::job_tag`].
pub(crate) mod job_result_tag {
    /// The CPU throttle counters of the job's control group (16 bytes: the periods, then the
    /// throttled periods, each a u64), written beside [`RESULT_USAGE`](crate::codec::job_tag::RESULT_USAGE) when the agent
    /// reported them. Read only beside [`RESULT_USAGE`](crate::codec::job_tag::RESULT_USAGE).
    pub const RESULT_CPU_THROTTLE: u16 = 60;
    /// The time some task of the group waited for a CPU (time), written beside
    /// [`RESULT_USAGE`](crate::codec::job_tag::RESULT_USAGE) when the agent reported it. Read only beside [`RESULT_USAGE`](crate::codec::job_tag::RESULT_USAGE).
    pub const RESULT_CPU_PRESSURE_SOME: u16 = 61;
    /// The most CPU the group used over a window of its run (16 bytes: the millicores, then the
    /// window in microseconds, each a u64), written beside
    /// [`RESULT_USAGE`](crate::codec::job_tag::RESULT_USAGE) when the agent reported it. Read only beside [`RESULT_USAGE`](crate::codec::job_tag::RESULT_USAGE).
    pub const RESULT_CPU_PEAK: u16 = 62;
}

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
    if let Some(used) = result.usage {
        row.put_opt(RESULT_CPU_THROTTLE, used.cpu_throttle.map(throttle));
        row.put_opt(
            RESULT_CPU_PRESSURE_SOME,
            used.cpu_pressure_some
                .map(|waited| time(StoreTime::from_duration(waited))),
        );
        row.put_opt(RESULT_CPU_PEAK, used.cpu_peak.map(peak));
    }
    row.put_opt(t::RESULT_LOGS, result.logs.map(lengths));
    row.put_opt(INPUT_FAILURE, result.input_failure.as_deref().map(text));
}

/// The result the row holds: `None` when it holds no reason.
pub(super) fn take_result(fields: &mut Fields) -> Result<Option<JobResult>, DecodeError> {
    let Some(reason) = fields.opt(t::RESULT_REASON, reason_of)? else {
        return Ok(None);
    };
    let usage = fields.opt(t::RESULT_USAGE, usage_of)?;
    let cpu_throttle = fields.opt(RESULT_CPU_THROTTLE, throttle_of)?;
    let cpu_peak = fields.opt(RESULT_CPU_PEAK, peak_of)?;
    let cpu_pressure_some = fields
        .opt(RESULT_CPU_PRESSURE_SOME, time_of)?
        .map(StoreTime::as_duration);
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
        usage: usage.map(|used| ResourceUsage {
            cpu_throttle,
            cpu_pressure_some,
            cpu_peak,
            ..used
        }),
        logs: fields.opt(t::RESULT_LOGS, lengths_of)?,
        input_failure: fields.opt(INPUT_FAILURE, text_of)?,
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

/// A throttle value: the periods, then the throttled periods, each a `u64`.
fn throttle(throttle: CpuThrottle) -> Vec<u8> {
    let mut out = throttle.periods.to_be_bytes().to_vec();
    out.extend_from_slice(&throttle.throttled_periods.to_be_bytes());
    out
}

fn throttle_of(tag: u16, value: Vec<u8>) -> Result<CpuThrottle, DecodeError> {
    let (periods, throttled) = value
        .split_at_checked(8)
        .ok_or(DecodeError::BadLength { tag })?;
    Ok(CpuThrottle {
        periods: u64_of(tag, periods.to_vec())?,
        throttled_periods: u64_of(tag, throttled.to_vec())?,
    })
}

/// A peak value: the millicores, then the window in microseconds, each a `u64`.
fn peak(peak: CpuPeak) -> Vec<u8> {
    let mut out = peak.millicores.to_be_bytes().to_vec();
    out.extend_from_slice(&peak.window_usec.to_be_bytes());
    out
}

fn peak_of(tag: u16, value: Vec<u8>) -> Result<CpuPeak, DecodeError> {
    let (millicores, window) = value
        .split_at_checked(8)
        .ok_or(DecodeError::BadLength { tag })?;
    Ok(CpuPeak {
        millicores: u64_of(tag, millicores.to_vec())?,
        window_usec: u64_of(tag, window.to_vec())?,
    })
}

fn usage_of(tag: u16, value: Vec<u8>) -> Result<ResourceUsage, DecodeError> {
    let (cpu, peak) = value
        .split_at_checked(TIME_LEN)
        .ok_or(DecodeError::BadLength { tag })?;
    Ok(ResourceUsage {
        cpu_time: time_of(tag, cpu.to_vec())?.as_duration(),
        peak_memory_bytes: u64_of(tag, peak.to_vec())?,
        cpu_throttle: None,
        cpu_pressure_some: None,
        cpu_peak: None,
    })
}

fn reason_code(reason: ResultReason) -> u8 {
    match reason {
        ResultReason::Exit => 0,
        ResultReason::WallLimit => 1,
        ResultReason::Memory => 2,
        ResultReason::Cancelled => 3,
        ResultReason::InputFailed => 4,
    }
}

fn reason_of(tag: u16, value: Vec<u8>) -> Result<ResultReason, DecodeError> {
    match byte_of(tag, &value)? {
        0 => Ok(ResultReason::Exit),
        1 => Ok(ResultReason::WallLimit),
        2 => Ok(ResultReason::Memory),
        3 => Ok(ResultReason::Cancelled),
        4 => Ok(ResultReason::InputFailed),
        _ => Err(DecodeError::Newer),
    }
}

#[cfg(test)]
#[path = "result_fields_tests.rs"]
mod tests;
