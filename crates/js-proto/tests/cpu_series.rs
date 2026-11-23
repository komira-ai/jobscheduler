//! The CPU series of a usage record (`UsageRecord.cpu_series`, field 23), held to the evolution
//! rule of the package: it is an addition, so a reader of the previous release meets it as an
//! unknown field and the reader of this release reads a record from before it with none.

use js_proto::v1::{CpuSeries, UsageRecord};
use prost::Message;

/// The previous release's `UsageRecord`, to the last field it had (22). A fixture standing for
/// readers already deployed; it must not follow later changes.
#[derive(Clone, PartialEq, Message)]
struct PreviousUsageRecord {
    #[prost(string, tag = "1")]
    job_id: String,
    #[prost(string, tag = "2")]
    host_id: String,
    #[prost(uint64, tag = "3")]
    queue_wait_ms: u64,
    #[prost(uint64, tag = "4")]
    run_ms: u64,
    #[prost(uint64, tag = "7")]
    ended_at_ms: u64,
    #[prost(uint32, tag = "21")]
    mark_give_ups: u32,
    #[prost(uint64, optional, tag = "22")]
    estimated_run_ms: Option<u64>,
}

/// A record with a series.
fn record() -> UsageRecord {
    UsageRecord {
        job_id: "job-1".to_owned(),
        host_id: "host-a".to_owned(),
        queue_wait_ms: 40,
        run_ms: 5_500,
        ended_at_ms: 1_000_000,
        mark_give_ups: 2,
        estimated_run_ms: Some(6_000),
        cpu_series: Some(CpuSeries {
            granularity_secs: 2,
            points: vec![0, 16, 40],
            run_usec: 5_500_000,
            partial: true,
        }),
        ..UsageRecord::default()
    }
}

/// Asserts that a reader of the previous release decodes a record with a series, reading every
/// field it knows as sent, and that this release reads what that reader writes back (the series
/// is an unknown field, which the previous message type drops) with no series. Catches a field
/// number reused or a new field that breaks a previous reader.
#[test]
fn a_previous_reader_meets_a_record_with_a_series() {
    let bytes = record().encode_to_vec();
    let old = PreviousUsageRecord::decode(bytes.as_slice()).expect("the previous reader decodes");
    assert_eq!(
        old,
        PreviousUsageRecord {
            job_id: "job-1".to_owned(),
            host_id: "host-a".to_owned(),
            queue_wait_ms: 40,
            run_ms: 5_500,
            ended_at_ms: 1_000_000,
            mark_give_ups: 2,
            estimated_run_ms: Some(6_000),
        }
    );
    let read = UsageRecord::decode(old.encode_to_vec().as_slice()).expect("this release decodes");
    assert_eq!(read.cpu_series, None);
    assert_eq!(read.job_id, "job-1");
    assert_eq!(read.estimated_run_ms, Some(6_000));
}

/// Asserts that this release reads a record with a series as written, field for field, and a
/// record without one as having none, and that a series that is not partial sends no flag.
/// Catches a field read into another and a series invented for a record that has none.
#[test]
fn this_release_reads_a_series_as_sent() {
    let sent = record();
    let back = UsageRecord::decode(sent.encode_to_vec().as_slice()).expect("decodes");
    assert_eq!(back, sent);

    let none = UsageRecord {
        cpu_series: None,
        ..record()
    };
    let back = UsageRecord::decode(none.encode_to_vec().as_slice()).expect("decodes");
    assert_eq!(back.cpu_series, None);

    let whole = CpuSeries {
        granularity_secs: 1,
        points: vec![16],
        run_usec: 1_000_000,
        partial: false,
    };
    assert_eq!(whole.encode_to_vec(), [8, 1, 18, 1, 16, 24, 192, 132, 61]);
}
