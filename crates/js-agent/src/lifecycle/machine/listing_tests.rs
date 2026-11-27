use js_agent_wire::{
    CpuPeak, CpuSeries, CpuThrottle, HeldJob, HeldRun, HeldState, JobResult, JobUsage, LogReport,
    MemorySeries, Outcome, RunResult, limits,
};

use super::{ENVELOPE_MARGIN, LISTED_BUDGET, RESULT_RESERVE, SHORTEST_ENTRY, entry_len};
use crate::lifecycle::machine::tails::len_of;

/// `,"result":`, the key a job's result is listed under, and the comma after it.
const KEY_AND_COMMA: usize = 11;

fn id(prefix: char) -> String {
    format!("{prefix}{}", "0".repeat(limits::MAX_ID_LEN - 1))
}

/// Free text at its cap whose every character serialises as two bytes (`\"`).
fn escaped_text() -> String {
    "\"".repeat(limits::MAX_DETAIL_LEN)
}

/// Asserts that a job's result without its log tail, at every cap (the longest ids, a
/// `detail` and an `input_failure` of escaped characters, every number at its longest, full
/// usage and both series at their most points), weighs at most [`RESULT_RESERVE`] with its
/// key and comma, and a run's result too. The entries leave that much room, so the oldest
/// result always goes out. It catches a reserve smaller than a real result, with which a
/// heartbeat would carry no result at all and the agent never report an end.
#[test]
fn the_largest_result_without_a_tail_fits_the_reserve() {
    let job = JobResult {
        job_id: id('j'),
        allocation_id: id('a'),
        outcome: Outcome::FailedToStart,
        exit_code: Some(i32::MIN),
        detail: Some(escaped_text()),
        log_tail: None,
        logs: Some(LogReport {
            stdout_bytes: u64::MAX,
            stderr_bytes: u64::MAX,
            stdout_dropped: u64::MAX,
            stderr_dropped: u64::MAX,
        }),
        input_failure: Some(escaped_text()),
        usage: Some(JobUsage {
            peak_memory_bytes: u64::MAX,
            cpu_usage_usec: u64::MAX,
            cpu_throttle: Some(CpuThrottle {
                periods: u64::MAX,
                throttled_periods: u64::MAX,
            }),
            cpu_pressure_some_usec: Some(u64::MAX),
            cpu_peak: Some(CpuPeak {
                millicores: u64::MAX,
                window_usec: u64::MAX,
            }),
            peak_swap_bytes: Some(u64::MAX),
        }),
        cpu_series: Some(CpuSeries {
            granularity_secs: u32::MAX,
            points: vec![u16::MAX; js_agent_wire::CPU_SERIES_MAX_POINTS],
            run_usec: u64::MAX,
            partial: true,
        }),
        memory_series: Some(MemorySeries {
            granularity_secs: u32::MAX,
            points: vec![u32::MAX; js_agent_wire::MEMORY_SERIES_MAX_POINTS],
            run_usec: u64::MAX,
            partial: true,
        }),
    };
    let run = RunResult {
        run_id: id('r'),
        lease_id: id('l'),
        outcome: Outcome::FailedToStart,
        exit_code: Some(i32::MIN),
        detail: Some(escaped_text()),
        log_tail: None,
    };
    let job_len = len_of(&job) + KEY_AND_COMMA;
    assert!(job_len <= RESULT_RESERVE, "{job_len} bytes");
    assert!(
        len_of(&run) < job_len,
        "a run's result weighs less than a job's"
    );
}

/// Asserts that the entry [`entry_len`] measures is the longest of any state, for a run and
/// for a job without its result, at the longest id: `{"run_id":"…","state":"exited_unacked"}`
/// and its comma, 167 bytes, and 40 at a one-byte id. It catches an entry measured at a shorter state than it can
/// reach, with which the entries could pass their budget once their jobs end.
#[test]
fn an_entry_is_measured_at_its_longest_state() {
    let held = id('r');
    assert_eq!(entry_len(&held), 167);
    assert_eq!(entry_len("x"), SHORTEST_ENTRY);
    for state in [
        HeldState::Starting,
        HeldState::Running,
        HeldState::ExitedUnacked,
        HeldState::Unknown,
    ] {
        let run = HeldRun {
            run_id: held.clone(),
            state,
        };
        let job = HeldJob {
            job_id: held.clone(),
            state,
            result: None,
        };
        assert!(len_of(&run) < entry_len(&held), "{state:?}");
        assert!(len_of(&job) < entry_len(&held), "{state:?}");
    }
}

/// Asserts the arithmetic of the bound: the body less the load, the envelope and one result
/// at its largest leaves 24,576 bytes for the entries, 147 at the longest ids. It catches a
/// budget that leaves no room for the result the entries are held back for.
#[test]
fn the_entries_leave_the_load_the_envelope_and_one_result() {
    assert_eq!(
        LISTED_BUDGET + RESULT_RESERVE + ENVELOPE_MARGIN + limits::MAX_LOAD_BYTES,
        limits::HEARTBEAT_BODY_BUDGET
    );
    assert_eq!(LISTED_BUDGET / entry_len(&id('r')), 147);
}
