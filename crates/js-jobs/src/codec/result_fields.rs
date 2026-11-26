//! The result fields of a job row, tags 31 (the reason) to 40 (the usage), 43 (the stream
//! lengths), 58 (the code of an input that failed, `rootfs_fields`), 60 to 62 (the CPU
//! throttle counters, the CPU pressure and the CPU peak of the usage) and 64 (the CPU series).
//! They are written only when the job has
//! a result, and the reason is what says it has one: a row without it, as every row written before results were stored, reads with no
//! result. A result without tag 43, as every result written before the lengths were stored,
//! reads with none; a build from before tag 43 keeps it as a field it does not know and writes
//! it back unchanged. Tags 60 to 62 are written only for a usage that has the value, and read
//! only beside tag 40: a row with one of them and no tag 40 reads with no usage. A usage
//! without them, as every one written before them, reads with `None`; a build from before
//! them keeps them as fields it does not know and writes them back unchanged.
//!
//! Tag 64 is the job's CPU series as its host reported it: written when the result has a
//! well-formed one, whether or not it has a usage, and read whenever the row holds it. A result
//! without it, as every one written before it, reads with none; a build from before it keeps it
//! as a field it does not know and writes it back unchanged. Its value is the granularity in
//! seconds (`u16`), the run length in microseconds (`u64`), the partial flag (a byte, 0 or 1),
//! then each point (`u16`), all big-endian: 11 bytes and two for each point, so 267 bytes for
//! the most a series holds (128 points).
//!
//! Tag 65 is the job's non-file memory series as its host reported it, written and read as tag
//! 64 is, and kept the same way by a build from before it. Its value has the same 11-byte head,
//! then each point (`u32`, MiB), big-endian: 523 bytes for 128 points.

use js_state::StoreTime;
use js_types::{CpuSeries, MemorySeries};

use self::job_result_tag::{
    RESULT_CPU_PEAK, RESULT_CPU_PRESSURE_SOME, RESULT_CPU_SERIES, RESULT_CPU_THROTTLE,
    RESULT_MEMORY_SERIES,
};
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

/// The bytes of a series value before its points: the granularity (`u16`), the run length
/// (`u64`) and the partial flag (a byte).
const SERIES_HEAD_LEN: usize = 11;

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
    /// The job's CPU series (11 bytes: the granularity in seconds as a u16, the run length in
    /// microseconds as a u64 and the partial flag as a byte, then each point as a u16), written
    /// when the result has a well-formed one. Read whether or not the result has a usage.
    pub const RESULT_CPU_SERIES: u16 = 64;
    /// The job's non-file memory series (the 11-byte head of [`RESULT_CPU_SERIES`], then each
    /// point as a u32 of MiB), written when the result has a well-formed one. Read whether or
    /// not the result has a usage.
    pub const RESULT_MEMORY_SERIES: u16 = 65;
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
    row.put_opt(
        RESULT_CPU_SERIES,
        result
            .cpu_series
            .as_ref()
            .filter(|series| series.is_well_formed())
            .map(series),
    );
    row.put_opt(
        RESULT_MEMORY_SERIES,
        result
            .memory_series
            .as_ref()
            .filter(|series| series.is_well_formed())
            .map(memory_series),
    );
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
        cpu_series: fields.opt(RESULT_CPU_SERIES, series_of)?,
        memory_series: fields.opt(RESULT_MEMORY_SERIES, memory_series_of)?,
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

/// A series value: the granularity (`u16`), the run length (`u64`), the partial flag (a byte),
/// then each point (`u16`), all big-endian.
fn series(series: &CpuSeries) -> Vec<u8> {
    let mut out = u16::try_from(series.granularity_secs)
        .unwrap_or_default()
        .to_be_bytes()
        .to_vec();
    out.extend_from_slice(&series.run_usec.to_be_bytes());
    out.push(u8::from(series.partial));
    out.extend(series.points.iter().flat_map(|point| point.to_be_bytes()));
    out
}

fn series_of(tag: u16, value: Vec<u8>) -> Result<CpuSeries, DecodeError> {
    let (head, points) = value
        .split_first_chunk::<SERIES_HEAD_LEN>()
        .ok_or(DecodeError::BadLength { tag })?;
    if points.len() % 2 != 0 {
        return Err(DecodeError::BadLength { tag });
    }
    let [g0, g1, r0, r1, r2, r3, r4, r5, r6, r7, partial] = *head;
    let (pairs, _) = points.as_chunks::<2>();
    let points = pairs.iter().map(|pair| u16::from_be_bytes(*pair)).collect();
    CpuSeries::new(
        u32::from(u16::from_be_bytes([g0, g1])),
        points,
        u64::from_be_bytes([r0, r1, r2, r3, r4, r5, r6, r7]),
        bool_of(tag, vec![partial])?,
    )
    .ok_or(DecodeError::BadValue { tag })
}

/// A memory series value: the head of a [`series`] value, then each point (`u32`), all
/// big-endian.
fn memory_series(series: &MemorySeries) -> Vec<u8> {
    let mut out = u16::try_from(series.granularity_secs)
        .unwrap_or_default()
        .to_be_bytes()
        .to_vec();
    out.extend_from_slice(&series.run_usec.to_be_bytes());
    out.push(u8::from(series.partial));
    out.extend(series.points.iter().flat_map(|point| point.to_be_bytes()));
    out
}

fn memory_series_of(tag: u16, value: Vec<u8>) -> Result<MemorySeries, DecodeError> {
    let (head, points) = value
        .split_first_chunk::<SERIES_HEAD_LEN>()
        .ok_or(DecodeError::BadLength { tag })?;
    if points.len() % 4 != 0 {
        return Err(DecodeError::BadLength { tag });
    }
    let [g0, g1, r0, r1, r2, r3, r4, r5, r6, r7, partial] = *head;
    let (words, _) = points.as_chunks::<4>();
    let points = words.iter().map(|word| u32::from_be_bytes(*word)).collect();
    MemorySeries::new(
        u32::from(u16::from_be_bytes([g0, g1])),
        points,
        u64::from_be_bytes([r0, r1, r2, r3, r4, r5, r6, r7]),
        bool_of(tag, vec![partial])?,
    )
    .ok_or(DecodeError::BadValue { tag })
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

pub(super) fn reason_code(reason: ResultReason) -> u8 {
    match reason {
        ResultReason::Exit => 0,
        ResultReason::WallLimit => 1,
        ResultReason::Memory => 2,
        ResultReason::Cancelled => 3,
        ResultReason::InputFailed => 4,
    }
}

pub(super) fn reason_of(tag: u16, value: Vec<u8>) -> Result<ResultReason, DecodeError> {
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
