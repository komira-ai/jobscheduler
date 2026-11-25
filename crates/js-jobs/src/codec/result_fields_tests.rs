//! The CPU throttle counters, the CPU pressure and the CPU peak of a usage (tags 60 to 62): each is stored
//! under its own tag when the usage has it, a usage from before them reads with
//! `None`, and the tags are read only beside tag 40; and the CPU series of a result (tag 64):
//! its bytes, its size, a row from before it, and the values refused; and the memory series
//! (tag 65), the same ways.

use std::time::Duration;

use crate::codec::result_fields::job_result_tag;
use crate::codec::tests::{full_job, full_result, job_row, without};
use crate::codec::{Carried, DecodeError, decode_job, field_of, job_tag, set_field};
use crate::record::JobRecord;
use crate::result::{CpuPeak, CpuThrottle, JobResult, ResourceUsage};
use js_types::{CpuSeries, MemorySeries};

/// The tags of the CPU throttle counters and the CPU pressure of a usage.
const CPU_TAGS: [u16; 3] = [
    job_result_tag::RESULT_CPU_THROTTLE,
    job_result_tag::RESULT_CPU_PRESSURE_SOME,
    job_result_tag::RESULT_CPU_PEAK,
];

/// A job whose usage has the given CPU counters.
fn job_with(cpu_throttle: Option<CpuThrottle>, cpu_pressure_some: Option<Duration>) -> JobRecord {
    JobRecord {
        result: Some(JobResult {
            usage: Some(ResourceUsage {
                cpu_throttle,
                cpu_pressure_some,
                cpu_peak: None,
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
        (job_with(None, None), [false; 3]),
        (
            job_with(
                Some(CpuThrottle {
                    periods: 0,
                    throttled_periods: 0,
                }),
                None,
            ),
            [true, false, false],
        ),
        (job_with(None, Some(Duration::ZERO)), [false, true, false]),
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
            cpu_peak: None,
        })
    );

    let no_usage = without(&job_row(&counted()), job_tag::RESULT_USAGE);
    let read = decode_job(&no_usage).expect("reads");
    assert_eq!(read.result.expect("a result").usage, None);
    assert_eq!(read.carried, Carried::default());
}

/// A job whose usage has the given CPU peak.
fn peaked(cpu_peak: Option<CpuPeak>) -> JobRecord {
    JobRecord {
        result: Some(JobResult {
            usage: Some(ResourceUsage {
                cpu_peak,
                ..full_result().usage.expect("a usage")
            }),
            ..full_result()
        }),
        ..full_job()
    }
}

// Asserts the CPU peak of a usage is stored under tag 62 as 16 bytes (the millicores, then the
// window in microseconds, each a u64 big-endian), that a usage without it writes no tag 62, that
// tag 40 keeps its 20 bytes either way, and that the peak reads back exactly. Catches a value
// written under another tag, the two numbers swapped, and a tag written for a `None`.
#[test]
fn the_cpu_peak_of_a_usage_is_stored_under_tag_62_when_reported() {
    let job = peaked(Some(CpuPeak {
        millicores: 3_500,
        window_usec: 5_000_000,
    }));
    let row = job_row(&job);
    assert_eq!(
        field_of(&row, 62),
        Some(
            [
                3_500u64.to_be_bytes().to_vec(),
                5_000_000u64.to_be_bytes().to_vec()
            ]
            .concat()
        )
    );
    assert_eq!(
        field_of(&row, job_tag::RESULT_USAGE).map(|v| v.len()),
        Some(20)
    );
    assert_eq!(decode_job(&row).expect("reads").result, job.result);

    let none = peaked(None);
    let row = job_row(&none);
    assert_eq!(field_of(&row, 62), None);
    assert_eq!(decode_job(&row).expect("reads").result, none.result);
}

// Asserts a usage row without tag 62, as every row before it, reads with no CPU peak and keeps
// the rest of its usage, and that tag 62 without tag 40 reads with no usage and is not carried.
// Catches a reader that refuses an older row and one that invents a usage from tag 62 alone.
#[test]
fn a_usage_row_without_tag_62_reads_with_no_cpu_peak() {
    let job = peaked(Some(CpuPeak {
        millicores: 1,
        window_usec: 2,
    }));
    let older = without(&job_row(&job), 62);
    let read = decode_job(&older).expect("reads").result.expect("a result");
    assert_eq!(read.usage, peaked(None).result.expect("a result").usage);

    let no_usage = without(&job_row(&job), job_tag::RESULT_USAGE);
    let read = decode_job(&no_usage).expect("reads");
    assert_eq!(read.result.expect("a result").usage, None);
    assert_eq!(read.carried, Carried::default());
}

/// A series of `points` at 2 s a point over a run of 5.5 s, partial as given.
fn series_of_points(points: Vec<u16>, partial: bool) -> CpuSeries {
    CpuSeries::new(2, points, 5_500_000, partial).expect("a well-formed series")
}

/// A job whose result has the given CPU series.
fn with_series(cpu_series: Option<CpuSeries>) -> JobRecord {
    JobRecord {
        result: Some(JobResult {
            cpu_series,
            ..full_result()
        }),
        ..full_job()
    }
}

/// Asserts the CPU series of a result is stored under tag 64 as the granularity (u16), the run
/// length in microseconds (u64), the partial flag (a byte) and each point (u16), all
/// big-endian, that a series that is not partial writes a 0 flag, that a result with none writes
/// no tag 64, and that each reads back exactly. Catches a value written under another tag, a
/// field swapped, the flag inverted, a width changed, and a tag written for a `None`.
#[test]
fn the_cpu_series_of_a_result_is_stored_under_tag_64() {
    let job = with_series(Some(series_of_points(vec![0, 16, 40], true)));
    let row = job_row(&job);
    assert_eq!(
        field_of(&row, 64),
        Some(
            [
                &[0, 2][..],
                &5_500_000u64.to_be_bytes(),
                &[1],
                &[0, 0, 0, 16, 0, 40]
            ]
            .concat()
        )
    );
    assert_eq!(decode_job(&row).expect("reads").result, job.result);

    let whole = with_series(Some(series_of_points(vec![65535], false)));
    let row = job_row(&whole);
    assert_eq!(
        field_of(&row, 64),
        Some(
            [
                &[0, 2][..],
                &5_500_000u64.to_be_bytes(),
                &[0],
                &[0xff, 0xff]
            ]
            .concat()
        )
    );
    assert_eq!(decode_job(&row).expect("reads").result, whole.result);

    let empty = with_series(Some(series_of_points(Vec::new(), false)));
    let row = job_row(&empty);
    assert_eq!(field_of(&row, 64).map(|value| value.len()), Some(11));
    assert_eq!(decode_job(&row).expect("reads").result, empty.result);

    let none = with_series(None);
    let row = job_row(&none);
    assert_eq!(field_of(&row, 64), None);
    assert_eq!(decode_job(&row).expect("reads").result, none.result);
}

/// Asserts the most a series holds, 128 points, is a 267-byte value: the 128 points and the
/// 2-byte granularity make 258 bytes, and the run length and the flag add the other 9.
/// Catches a width that grows a row past what a series is budgeted.
#[test]
fn a_full_series_is_267_bytes() {
    let full = series_of_points(vec![7; 128], false);
    let value = field_of(&job_row(&with_series(Some(full))), 64).expect("tag 64");
    assert_eq!(value.len(), 267);
    assert_eq!(value.len() - 8 - 1, 258);
}

/// Asserts a series is stored and read whether or not the result has a usage: with no tag 40 the
/// series still reads, and the usage reads `None`. Catches a series tied to the usage, as the
/// CPU counters are, which would drop a series an agent reported without one.
#[test]
fn a_series_is_kept_without_a_usage() {
    let job = JobRecord {
        result: Some(JobResult {
            usage: None,
            cpu_series: Some(series_of_points(vec![1, 2], false)),
            ..full_result()
        }),
        ..full_job()
    };
    let row = job_row(&job);
    assert_eq!(field_of(&row, job_tag::RESULT_USAGE), None);
    assert!(field_of(&row, 64).is_some());
    assert_eq!(decode_job(&row).expect("reads").result, job.result);
}

/// Asserts a row from before tag 64 (every field but the series) reads with no series and keeps
/// the rest of its result, and carries nothing. Catches a reader that refuses a row an older
/// build wrote, or one that invents a series.
#[test]
fn a_row_from_before_tag_64_reads_with_no_series() {
    let job = with_series(Some(series_of_points(vec![3], false)));
    let older = without(&job_row(&job), 64);
    let read = decode_job(&older).expect("reads");
    assert_eq!(read.result, with_series(None).result);
    assert_eq!(read.carried, Carried::default());
}

/// Asserts a series that is not well formed is not written, so the row reads with none: a
/// granularity that is not a power of two, one that does not fit 16 bits, and 129 points.
/// Catches a row written that this build then refuses to read.
#[test]
fn a_series_that_is_not_well_formed_is_not_stored() {
    for (granularity_secs, points) in [(3, 1), (1 << 16, 1), (0, 1), (1, 129)] {
        let series = CpuSeries {
            granularity_secs,
            points: vec![1; points],
            run_usec: 1,
            partial: false,
        };
        let row = job_row(&with_series(Some(series)));
        assert_eq!(field_of(&row, 64), None, "{granularity_secs} {points}");
        assert_eq!(
            decode_job(&row).expect("reads").result,
            with_series(None).result
        );
    }
}

/// Asserts a tag 64 that is too short, has half a point, has a partial flag that is not 0 or 1,
/// a granularity that is not a power of two (or is zero), or more than 128 points is refused
/// with the tag named, and the row does not read, while the widest series (128 points at 32768 s)
/// reads. Catches a reader that accepts a series no agent writes.
#[test]
fn a_bad_series_value_is_refused() {
    let value = |granularity: u16, partial: u8, points: usize| {
        [
            &granularity.to_be_bytes()[..],
            &1u64.to_be_bytes(),
            &[partial],
            &vec![0; points * 2],
        ]
        .concat()
    };
    let row_with = |value: Vec<u8>| {
        set_field(&job_row(&with_series(None)), 64, &value).expect("the row parses")
    };
    let refused = |value: Vec<u8>| decode_job(&row_with(value)).expect_err("refused");
    assert_eq!(refused(vec![0; 10]), DecodeError::BadLength { tag: 64 });
    assert_eq!(refused(Vec::new()), DecodeError::BadLength { tag: 64 });
    let mut odd = value(1, 0, 2);
    odd.push(0);
    assert_eq!(refused(odd), DecodeError::BadLength { tag: 64 });
    assert_eq!(refused(value(1, 2, 2)), DecodeError::BadValue { tag: 64 });
    assert_eq!(refused(value(3, 0, 2)), DecodeError::BadValue { tag: 64 });
    assert_eq!(refused(value(0, 0, 2)), DecodeError::BadValue { tag: 64 });
    assert_eq!(refused(value(1, 0, 129)), DecodeError::BadValue { tag: 64 });
    let widest = decode_job(&row_with(value(32768, 1, 128))).expect("reads");
    let series = widest
        .result
        .expect("a result")
        .cpu_series
        .expect("a series");
    assert_eq!((series.granularity_secs, series.points.len()), (32768, 128));
}

/// A memory series of `points` MiB at 4 s a point over a run of 9.5 s, partial as given.
fn memory_of_points(points: Vec<u32>, partial: bool) -> MemorySeries {
    MemorySeries::new(4, points, 9_500_000, partial).expect("a well-formed series")
}

/// A job whose result has the given memory series.
fn with_memory(memory_series: Option<MemorySeries>) -> JobRecord {
    JobRecord {
        result: Some(JobResult {
            memory_series,
            ..full_result()
        }),
        ..full_job()
    }
}

/// Asserts the memory series of a result is stored under tag 65 as the granularity (u16), the
/// run length in microseconds (u64), the partial flag (a byte) and each point (u32 of MiB), all
/// big-endian, that a result with none writes no tag 65, that a series beside a CPU series does
/// not disturb tag 64, and that each reads back exactly. Catches a value written under another
/// tag, a point kept at 16 bits, a field swapped, the flag inverted, and a tag written for a
/// `None`.
#[test]
fn the_memory_series_of_a_result_is_stored_under_tag_65() {
    let job = with_memory(Some(memory_of_points(vec![0, 70_000, 5], true)));
    let row = job_row(&job);
    assert_eq!(
        field_of(&row, 65),
        Some(
            [
                &[0, 4][..],
                &9_500_000u64.to_be_bytes(),
                &[1],
                &[0, 0, 0, 0],
                &70_000u32.to_be_bytes(),
                &[0, 0, 0, 5],
            ]
            .concat()
        )
    );
    assert_eq!(
        field_of(&row, 64),
        field_of(&job_row(&with_memory(None)), 64)
    );
    assert_eq!(decode_job(&row).expect("reads").result, job.result);

    let whole = with_memory(Some(memory_of_points(vec![u32::MAX], false)));
    let row = job_row(&whole);
    assert_eq!(field_of(&row, 65).map(|value| value[10]), Some(0));
    assert_eq!(decode_job(&row).expect("reads").result, whole.result);

    let empty = with_memory(Some(memory_of_points(Vec::new(), false)));
    let row = job_row(&empty);
    assert_eq!(field_of(&row, 65).map(|value| value.len()), Some(11));
    assert_eq!(decode_job(&row).expect("reads").result, empty.result);

    let none = with_memory(None);
    let row = job_row(&none);
    assert_eq!(field_of(&row, 65), None);
    assert_eq!(decode_job(&row).expect("reads").result, none.result);
}

/// Asserts the most a memory series holds, 128 points, is a 523-byte value: the 11-byte head
/// and 4 bytes a point. Catches a width that grows a row past what a series is budgeted.
#[test]
fn a_full_memory_series_is_523_bytes() {
    let full = memory_of_points(vec![7; 128], false);
    let value = field_of(&job_row(&with_memory(Some(full))), 65).expect("tag 65");
    assert_eq!(value.len(), 523);
}

/// Asserts a memory series is stored and read whether or not the result has a usage, and beside
/// a CPU series: with no tag 40 it still reads. Catches a series tied to the usage, which would
/// drop one an agent reported without it.
#[test]
fn a_memory_series_is_kept_without_a_usage() {
    let job = JobRecord {
        result: Some(JobResult {
            usage: None,
            cpu_series: Some(series_of_points(vec![1], false)),
            memory_series: Some(memory_of_points(vec![3, 4], false)),
            ..full_result()
        }),
        ..full_job()
    };
    let row = job_row(&job);
    assert_eq!(field_of(&row, job_tag::RESULT_USAGE), None);
    assert!(field_of(&row, 65).is_some());
    assert_eq!(decode_job(&row).expect("reads").result, job.result);
}

/// Asserts a row from before tag 65 reads with no memory series, keeps the rest of its result
/// (the CPU series included) and carries nothing. Catches a reader that refuses a row an older
/// build wrote, or invents a series.
#[test]
fn a_row_from_before_tag_65_reads_with_no_memory_series() {
    let job = JobRecord {
        result: Some(JobResult {
            cpu_series: Some(series_of_points(vec![3], false)),
            memory_series: Some(memory_of_points(vec![3], false)),
            ..full_result()
        }),
        ..full_job()
    };
    let older = without(&job_row(&job), 65);
    let read = decode_job(&older).expect("reads");
    assert_eq!(
        read.result,
        Some(JobResult {
            cpu_series: Some(series_of_points(vec![3], false)),
            memory_series: None,
            ..full_result()
        })
    );
    assert_eq!(read.carried, Carried::default());
}

/// Asserts a memory series that is not well formed is not written, so the row reads with none:
/// a granularity that is not a power of two, one that does not fit 16 bits, zero, and 129
/// points. Catches a row written that this build then refuses to read.
#[test]
fn a_memory_series_that_is_not_well_formed_is_not_stored() {
    for (granularity_secs, points) in [(3, 1), (1 << 16, 1), (0, 1), (1, 129)] {
        let series = MemorySeries {
            granularity_secs,
            points: vec![1; points],
            run_usec: 1,
            partial: false,
        };
        let row = job_row(&with_memory(Some(series)));
        assert_eq!(field_of(&row, 65), None, "{granularity_secs} {points}");
        assert_eq!(
            decode_job(&row).expect("reads").result,
            with_memory(None).result
        );
    }
}

/// Asserts a tag 65 that is too short, has a part of a point, has a partial flag that is not 0
/// or 1, a granularity that is not a power of two (or is zero), or more than 128 points is
/// refused with the tag named, and the row does not read, while the widest series (128 points
/// at 32768 s) reads. Catches a reader that accepts a series no agent writes.
#[test]
fn a_bad_memory_series_value_is_refused() {
    let value = |granularity: u16, partial: u8, points: usize| {
        [
            &granularity.to_be_bytes()[..],
            &1u64.to_be_bytes(),
            &[partial],
            &vec![0; points * 4],
        ]
        .concat()
    };
    let row_with = |value: Vec<u8>| {
        set_field(&job_row(&with_memory(None)), 65, &value).expect("the row parses")
    };
    let refused = |value: Vec<u8>| decode_job(&row_with(value)).expect_err("refused");
    assert_eq!(refused(vec![0; 10]), DecodeError::BadLength { tag: 65 });
    assert_eq!(refused(Vec::new()), DecodeError::BadLength { tag: 65 });
    for extra in 1..4 {
        let mut odd = value(1, 0, 2);
        odd.extend(vec![0; extra]);
        assert_eq!(refused(odd), DecodeError::BadLength { tag: 65 });
    }
    assert_eq!(refused(value(1, 2, 2)), DecodeError::BadValue { tag: 65 });
    assert_eq!(refused(value(3, 0, 2)), DecodeError::BadValue { tag: 65 });
    assert_eq!(refused(value(0, 0, 2)), DecodeError::BadValue { tag: 65 });
    assert_eq!(refused(value(1, 0, 129)), DecodeError::BadValue { tag: 65 });
    let widest = decode_job(&row_with(value(32768, 1, 128))).expect("reads");
    let series = widest
        .result
        .expect("a result")
        .memory_series
        .expect("a series");
    assert_eq!((series.granularity_secs, series.points.len()), (32768, 128));
}
