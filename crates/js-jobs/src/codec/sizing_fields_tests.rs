//! The sizing history of an allocation row (tag 16) and the throttle counts of a job's result
//! (tag 60): each round-trips beside a field a later build wrote, an older row without it reads
//! as none, a value this build does not understand reads as none and never fails the row, and
//! the layout is pinned byte for byte.

use crate::codec::tests::{allocation, full_job, job_row, shape, with, without};
use crate::codec::{decode_allocation, decode_job, encode_allocation, field_of};
use crate::record::{AllocationRecord, JobRecord, SizedFrom};
use crate::result::{CpuThrottle, JobResult, ResourceUsage};

const SIZED_FROM: u16 = 16;
const CPU_THROTTLE: u16 = 60;

fn allocation_row(allocation: &AllocationRecord) -> Vec<u8> {
    encode_allocation(allocation).expect("encodes")
}

fn sized(from: Option<SizedFrom>) -> AllocationRecord {
    AllocationRecord {
        sized_from: from,
        ..allocation(shape())
    }
}

/// A full job whose result's usage has `throttle`.
fn throttled(throttle: Option<CpuThrottle>) -> JobRecord {
    let mut job = full_job();
    let result = job.result.take().expect("a full job has a result");
    let usage = ResourceUsage {
        cpu_throttle: throttle,
        ..result.usage.expect("a full result has usage")
    };
    job.result = Some(JobResult {
        usage: Some(usage),
        ..result
    });
    job
}

fn count(periods: u64, throttled_periods: u64) -> CpuThrottle {
    CpuThrottle {
        periods,
        throttled_periods,
    }
}

/// Every history reads back as written, as one byte of its own (1 class, 2 fallback, 3 cold,
/// 4 over-cap), and none writes no field. It catches two histories sharing a byte, a history
/// lost on the way through the row, and a none written as a value.
#[test]
fn a_sized_from_round_trips_as_one_byte_per_history() {
    let cases = [
        (SizedFrom::Class, 1),
        (SizedFrom::Fallback, 2),
        (SizedFrom::Cold, 3),
        (SizedFrom::OverCap, 4),
    ];
    for (from, byte) in cases {
        let row = allocation_row(&sized(Some(from)));
        assert_eq!(field_of(&row, SIZED_FROM), Some(vec![byte]));
        assert_eq!(decode_allocation(&row), Ok(sized(Some(from))));
    }
    let row = allocation_row(&sized(None));
    assert_eq!(field_of(&row, SIZED_FROM), None);
    assert_eq!(decode_allocation(&row), Ok(sized(None)));
}

/// A row an older build wrote, with no sizing tag, reads with none. It catches a row from
/// before the field refused or read as having a history.
#[test]
fn an_allocation_row_without_the_sized_from_tag_reads_as_none() {
    let row = allocation_row(&sized(Some(SizedFrom::Class)));
    let older = without(&row, SIZED_FROM);
    assert_eq!(decode_allocation(&older), Ok(sized(None)));
}

/// A byte this build does not know, or a value of another length, reads as none and the row
/// still decodes. It catches a newer build's history failing an older reader's row.
#[test]
fn an_unknown_sized_from_value_reads_as_none() {
    let row = allocation_row(&sized(Some(SizedFrom::Class)));
    for value in [&[0u8][..], &[5], &[255], &[], &[1, 1]] {
        let odd = with(&row, SIZED_FROM, value);
        assert_eq!(decode_allocation(&odd), Ok(sized(None)), "{value:?}");
    }
}

/// The throttle counts read back as written, two big-endian `u64` (periods, then throttled
/// periods) at tag 60, including zero periods. It catches the two counts swapped, a count
/// dropped, and a zero read as absent.
#[test]
fn the_cpu_throttle_of_a_result_round_trips() {
    for throttle in [count(120, 7), count(0, 0), count(u64::MAX, 1)] {
        let job = throttled(Some(throttle));
        let row = job_row(&job);
        let mut want = throttle.periods.to_be_bytes().to_vec();
        want.extend_from_slice(&throttle.throttled_periods.to_be_bytes());
        assert_eq!(field_of(&row, CPU_THROTTLE), Some(want));
        assert_eq!(decode_job(&row), Ok(job));
    }
}

/// A usage with no counts writes no tag 60 and reads back with none, and a row an older build
/// wrote (the usage at tag 40 and no tag 60) reads its usage with none. It catches an absence
/// written as zeros and an older row refused.
#[test]
fn a_result_without_throttle_counts_reads_with_none() {
    let job = throttled(None);
    let row = job_row(&job);
    assert_eq!(field_of(&row, CPU_THROTTLE), None);
    assert_eq!(decode_job(&row), Ok(job));

    let with_counts = job_row(&throttled(Some(count(10, 2))));
    let older = without(&with_counts, CPU_THROTTLE);
    assert_eq!(decode_job(&older), Ok(throttled(None)));
}

/// Counts of the wrong length read as none and the row still decodes, with the rest of the
/// usage intact. It catches a short or long value failing the whole row.
#[test]
fn throttle_counts_of_the_wrong_length_read_as_none() {
    let row = job_row(&throttled(Some(count(10, 2))));
    for value in [&[][..], &[0; 8], &[0; 15], &[0; 17]] {
        let odd = with(&row, CPU_THROTTLE, value);
        assert_eq!(decode_job(&odd), Ok(throttled(None)), "{value:?}");
    }
}
