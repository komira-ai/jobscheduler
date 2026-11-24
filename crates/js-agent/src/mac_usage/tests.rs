//! A native run's usage from samples of its processes and its supervisor's wait.

use js_agent_wire::{CpuSeries, JobUsage, MemorySeries};

use super::{ProcUsage, RunUsage, Series, ticks_to_usec};
use crate::mac_supervisor::JobRusage;

const S: u64 = 1_000_000;
const MIB: u64 = 1 << 20;

/// A process that has used `cpu` microseconds and holds `mib` MiB.
fn process(cpu: u64, mib: u64) -> ProcUsage {
    ProcUsage {
        cpu_usec: cpu,
        footprint_bytes: mib * MIB,
    }
}

/// The usage a Mac reports: CPU and peak memory, none of the counters only Linux has.
fn used(cpu: u64, peak: u64) -> JobUsage {
    JobUsage {
        peak_memory_bytes: peak,
        cpu_usage_usec: cpu,
        cpu_throttle: None,
        cpu_pressure_some_usec: None,
        cpu_peak: None,
        peak_swap_bytes: None,
    }
}

/// A run started at 0 with both series, sampled at 1 s (two processes: 1 s of CPU and 100 MiB
/// together), at 2 s (one process: the other ended and was waited for, its time now its
/// parent's: 3 s and 300 MiB) and at 2.5 s (a process the run no longer parents ended, so the
/// sum falls to 2 s and 50 MiB), seen to end at 3 s.
fn sampled() -> RunUsage {
    let mut run = RunUsage::starting(
        0,
        Series {
            cpu: true,
            memory: true,
        },
    );
    run.observe(S, &[process(600_000, 60), process(400_000, 40)]);
    run.observe(2 * S, &[process(3 * S, 300)]);
    run.observe(5 * S / 2, &[process(2 * S, 50)]);
    run.end_at(3 * S);
    run
}

/// Asserts that a run's CPU is the most its processes had used together at a sample, which a
/// sum that fell does not take back, and its peak the most they held together at one sample;
/// and that the usage is, for each, the more of the samples and of the supervisor's wait, with
/// none of the Linux-only fields. Catches a CPU that falls when a process ends, a peak summed
/// over time or taken from one process, and the wait's figures ignored or always preferred.
#[test]
fn the_usage_is_the_more_of_the_samples_and_of_the_wait() {
    let run = sampled();
    assert_eq!(run.usage(None), Some(used(3 * S, 300 * MIB)));
    let more_cpu = JobRusage {
        cpu_usec: 4 * S,
        max_rss_bytes: 200 * MIB,
    };
    assert_eq!(run.usage(Some(more_cpu)), Some(used(4 * S, 300 * MIB)));
    let more_memory = JobRusage {
        cpu_usec: S,
        max_rss_bytes: 400 * MIB,
    };
    assert_eq!(run.usage(Some(more_memory)), Some(used(3 * S, 400 * MIB)));
}

/// Asserts that a run no sample saw reports what its wait said, and with no wait's usage
/// either reports none (never a zero); and that a sample of no process is a measured zero.
/// Catches an unknown usage reported as zero, which would teach its class a size of nothing.
#[test]
fn a_run_with_no_sample_reports_its_wait_or_nothing() {
    let run = RunUsage::starting(0, Series::default());
    assert_eq!(run.usage(None), None);
    let reaped = JobRusage {
        cpu_usec: 250_000,
        max_rss_bytes: 9 * MIB,
    };
    assert_eq!(run.usage(Some(reaped)), Some(used(250_000, 9 * MIB)));
    let mut empty = RunUsage::starting(0, Series::default());
    empty.observe(S, &[]);
    assert_eq!(empty.usage(None), Some(used(0, 0)));
}

/// Asserts the two series of the sampled run: the CPU per second in sixteenths of a core (the
/// CPU the wait added after the last sample in the last second, up to where the end was seen,
/// not up to the later time asked), and the memory per second in MiB, each the most of its
/// span (the fall at 2.5 s does not lower the third). Catches a series that ends where it is
/// read instead of where the run ended, the wait's CPU left out of it, and a memory series of
/// the last sample instead of the most.
#[test]
fn the_series_follow_the_samples_and_end_where_the_run_was_seen_to_end() {
    let run = sampled();
    let reaped = JobRusage {
        cpu_usec: 4 * S,
        max_rss_bytes: MIB,
    };
    assert_eq!(
        run.cpu_series(10 * S, Some(reaped)),
        Some(CpuSeries {
            granularity_secs: 1,
            points: vec![16, 32, 16],
            run_usec: 3 * S,
            partial: false,
        })
    );
    assert_eq!(
        run.cpu_series(10 * S, None),
        Some(CpuSeries {
            granularity_secs: 1,
            points: vec![16, 32, 0],
            run_usec: 3 * S,
            partial: false,
        })
    );
    assert_eq!(
        run.memory_series(10 * S),
        Some(MemorySeries {
            granularity_secs: 1,
            points: vec![0, 100, 300],
            run_usec: 3 * S,
            partial: false,
        })
    );
}

/// Asserts that a run records only the series it was asked for, and that a series with no
/// time is none. Catches a series sent that the agent was not told to record.
#[test]
fn a_run_records_only_the_series_it_was_asked_for() {
    let mut neither = RunUsage::starting(0, Series::default());
    neither.observe(S, &[process(S, 10)]);
    assert_eq!(neither.cpu_series(2 * S, None), None);
    assert_eq!(neither.memory_series(2 * S), None);
    let mut cpu_only = RunUsage::starting(
        0,
        Series {
            cpu: true,
            memory: false,
        },
    );
    cpu_only.observe(S, &[process(S, 10)]);
    assert!(cpu_only.cpu_series(S, None).is_some());
    assert_eq!(cpu_only.memory_series(S), None);
    let both = Series {
        cpu: true,
        memory: true,
    };
    let instant = RunUsage::starting(5 * S, both);
    assert_eq!(instant.cpu_series(5 * S, None), None);
    assert_eq!(instant.memory_series(5 * S), None);
}

/// Asserts the absolute-time ticks of a Mac as microseconds: Apple silicon's ratio of 125/3
/// (24 million ticks a second), an Intel Mac's 1/1 (ticks are nanoseconds), a zero denominator
/// taken as 1/1, and a value too large for microseconds saturating. Catches ticks taken for
/// nanoseconds on Apple silicon (a CPU 41 times too small) and a ratio applied upside down.
#[test]
fn absolute_time_ticks_are_microseconds_by_the_timebase() {
    assert_eq!(ticks_to_usec(24_000_000, 125, 3), 1_000_000);
    assert_eq!(ticks_to_usec(1_000_000_000, 1, 1), 1_000_000);
    assert_eq!(ticks_to_usec(1_999, 1, 1), 1);
    assert_eq!(ticks_to_usec(5_000, 7, 0), 5);
    assert_eq!(ticks_to_usec(u64::MAX, u32::MAX, 1), u64::MAX);
}
