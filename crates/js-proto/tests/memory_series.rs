//! The memory series of a usage record (`UsageRecord.memory_series`, field 25), held to the
//! evolution rule of the package: it is an addition, so a reader of the previous release meets
//! it as an unknown field and keeps every field it knows, and the reader of this release reads a
//! record from before it with none.

use js_proto::v1::{CpuSeries, MemorySeries, UsageRecord};
use prost::Message;

/// The previous release's `CpuSeries`, as its `UsageRecord` nests it.
#[derive(Clone, PartialEq, Message)]
struct PreviousCpuSeries {
    #[prost(uint32, tag = "1")]
    granularity_secs: u32,
    #[prost(uint32, repeated, tag = "2")]
    points: Vec<u32>,
    #[prost(uint64, tag = "3")]
    run_usec: u64,
    #[prost(bool, tag = "4")]
    partial: bool,
}

/// The previous release's `UsageRecord`, to the last field it had (24), with the CPU series and
/// the attempt. A fixture standing for readers already deployed (a `js` or a client built from
/// the previous release's schema); it must not follow later changes.
#[derive(Clone, PartialEq, Message)]
struct PreviousUsageRecord {
    #[prost(string, tag = "1")]
    job_id: String,
    #[prost(string, tag = "2")]
    host_id: String,
    #[prost(uint64, tag = "4")]
    run_ms: u64,
    #[prost(uint64, tag = "7")]
    ended_at_ms: u64,
    #[prost(message, optional, tag = "23")]
    cpu_series: Option<PreviousCpuSeries>,
    #[prost(uint32, optional, tag = "24")]
    attempt: Option<u32>,
}

/// A record with a CPU series, an attempt and a memory series whose points pass 16 bits.
fn record() -> UsageRecord {
    UsageRecord {
        job_id: "job-1".to_owned(),
        host_id: "host-a".to_owned(),
        run_ms: 5_500,
        ended_at_ms: 1_000_000,
        cpu_series: Some(CpuSeries {
            granularity_secs: 2,
            points: vec![0, 16, 40],
            run_usec: 5_500_000,
            partial: true,
        }),
        attempt: Some(2),
        memory_series: Some(MemorySeries {
            granularity_secs: 2,
            points: vec![1, 700, 70_000],
            run_usec: 5_500_000,
            partial: true,
        }),
        ..UsageRecord::default()
    }
}

/// Asserts that a reader of the previous release decodes a record with a memory series,
/// reading every field it knows as sent (the CPU series and the attempt beside it included),
/// and that this release reads what that reader writes back (the memory series is an unknown
/// field, which the previous message type drops) with no memory series and the rest kept.
/// Catches a field number reused (a memory series read as the CPU series or the attempt) and a
/// new field that breaks a previous reader.
#[test]
fn a_previous_reader_meets_a_record_with_a_memory_series() {
    let name = "a_previous_reader_meets_a_record_with_a_memory_series";
    let bytes = record().encode_to_vec();
    let old = PreviousUsageRecord::decode(bytes.as_slice()).expect("the previous reader decodes");
    assert_eq!(
        old,
        PreviousUsageRecord {
            job_id: "job-1".to_owned(),
            host_id: "host-a".to_owned(),
            run_ms: 5_500,
            ended_at_ms: 1_000_000,
            cpu_series: Some(PreviousCpuSeries {
                granularity_secs: 2,
                points: vec![0, 16, 40],
                run_usec: 5_500_000,
                partial: true,
            }),
            attempt: Some(2),
        },
        "{name}: the previous reader"
    );
    let read = UsageRecord::decode(old.encode_to_vec().as_slice()).expect("this release decodes");
    assert_eq!(
        read,
        UsageRecord {
            memory_series: None,
            ..record()
        },
        "{name}: read back"
    );
}

/// Asserts that this release reads a record with a memory series as written, field for field,
/// and a record from before the field (what a server of the previous release sends) as having
/// none; and pins the bytes: field 25 as a length-delimited message (`0xca 0x01`), its points
/// packed, and no flag for a series that is not partial. Catches a field read into another, a
/// renumbered field, a point narrowed to 16 bits and a series invented for a record that has
/// none.
#[test]
fn this_release_reads_a_memory_series_as_sent() {
    let name = "this_release_reads_a_memory_series_as_sent";
    let sent = record();
    let back = UsageRecord::decode(sent.encode_to_vec().as_slice()).expect("decodes");
    assert_eq!(back, sent, "{name}: with");

    let before = PreviousUsageRecord {
        job_id: "job-1".to_owned(),
        ..PreviousUsageRecord::default()
    };
    let back = UsageRecord::decode(before.encode_to_vec().as_slice()).expect("decodes");
    assert_eq!(back.memory_series, None, "{name}: from before");

    let alone = UsageRecord {
        job_id: "j".to_owned(),
        memory_series: Some(MemorySeries {
            granularity_secs: 1,
            points: vec![70_000],
            run_usec: 1_000_000,
            partial: false,
        }),
        ..UsageRecord::default()
    };
    assert_eq!(
        alone.encode_to_vec(),
        [
            0x0a, 0x01, b'j', 0xca, 0x01, 11, 8, 1, 0x12, 3, 0xf0, 0xa2, 0x04, 0x18, 0xc0, 0x84,
            0x3d
        ],
        "{name}: bytes"
    );
}
