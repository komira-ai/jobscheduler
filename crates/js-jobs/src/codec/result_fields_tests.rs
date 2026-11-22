//! The CPU throttle counters and the CPU pressure of a usage (tags 60 and 61): each is stored
//! under its own tag when the usage has it, a usage from before them reads with
//! `None`, and the tags are read only beside tag 40.

use std::time::Duration;

use crate::codec::result_fields::job_result_tag;
use crate::codec::tests::{full_job, full_result, job_row, without};
use crate::codec::{Carried, decode_job, field_of, job_tag};
use crate::record::JobRecord;
use crate::result::{CpuThrottle, JobResult, ResourceUsage};

/// The tags of the CPU throttle counters and the CPU pressure of a usage.
const CPU_TAGS: [u16; 2] = [
    job_result_tag::RESULT_CPU_THROTTLE,
    job_result_tag::RESULT_CPU_PRESSURE_SOME,
];

/// A job whose usage has the given CPU counters.
fn job_with(cpu_throttle: Option<CpuThrottle>, cpu_pressure_some: Option<Duration>) -> JobRecord {
    JobRecord {
        result: Some(JobResult {
            usage: Some(ResourceUsage {
                cpu_throttle,
                cpu_pressure_some,
                ..full_result().usage.expect("a usage")
            }),
            ..full_result()
        }),
        ..full_job()
    }
}

/// A job whose usage has both CPU counters.
fn counted() -> JobRecord {
    job_with(
        Some(CpuThrottle {
            periods: 700,
            throttled_periods: 35,
        }),
        Some(Duration::new(2, 500_000)),
    )
}

// Asserts the usage's CPU throttle counters and pressure are stored under tags 60 and 61 with
// the value each holds (16 bytes: the periods, then the throttled periods; then a time), that a usage without them writes none of the tags, and
// that tag 40 keeps its 20 bytes either way. Catches a value written under another tag, a tag
// written for a `None`, and a tag 40 that grew.
#[test]
fn the_cpu_counters_of_a_usage_are_stored_under_their_own_tags() {
    let job = counted();
    let row = job_row(&job);
    assert_eq!(
        field_of(&row, 60),
        Some([700u64.to_be_bytes().to_vec(), 35u64.to_be_bytes().to_vec()].concat())
    );
    assert_eq!(
        field_of(&row, 61),
        Some(
            [
                2u64.to_be_bytes().to_vec(),
                500_000u32.to_be_bytes().to_vec()
            ]
            .concat()
        )
    );
    assert_eq!(
        field_of(&row, job_tag::RESULT_USAGE).map(|v| v.len()),
        Some(20)
    );
    assert_eq!(decode_job(&row).expect("reads").result, job.result);

    for (job, written) in [
        (job_with(None, None), [false; 2]),
        (
            job_with(
                Some(CpuThrottle {
                    periods: 0,
                    throttled_periods: 0,
                }),
                None,
            ),
            [true, false],
        ),
        (job_with(None, Some(Duration::ZERO)), [false, true]),
    ] {
        let row = job_row(&job);
        for (tag, written) in CPU_TAGS.into_iter().zip(written) {
            assert_eq!(field_of(&row, tag).is_some(), written, "tag {tag}");
        }
        assert_eq!(
            field_of(&row, job_tag::RESULT_USAGE).map(|v| v.len()),
            Some(20)
        );
        assert_eq!(decode_job(&row).expect("reads").result, job.result);
    }
}

// Asserts a usage row from before the CPU tags, tag 40 alone, reads with `None` for each of
// them and keeps its CPU time and peak memory, and that the CPU tags without tag 40 read with
// no usage and are not carried. Catches a reader that refuses a row an older build wrote, and
// one that invents a usage from the CPU tags alone.
#[test]
fn a_usage_row_without_the_cpu_tags_reads_with_none() {
    let mut older = job_row(&counted());
    for tag in CPU_TAGS {
        older = without(&older, tag);
    }
    let read = decode_job(&older).expect("reads").result.expect("a result");
    assert_eq!(
        read.usage,
        Some(ResourceUsage {
            cpu_time: Duration::from_secs(7),
            peak_memory_bytes: 4096,
            cpu_throttle: None,
            cpu_pressure_some: None,
        })
    );

    let no_usage = without(&job_row(&counted()), job_tag::RESULT_USAGE);
    let read = decode_job(&no_usage).expect("reads");
    assert_eq!(read.result.expect("a result").usage, None);
    assert_eq!(read.carried, Carried::default());
}
