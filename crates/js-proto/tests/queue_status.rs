//! What `Job` tells of why a job waits (its queue status, attempts and events), held to the
//! evolution rule of the package: the fields are additions, numbered after the shipped ones,
//! the bytes they add are pinned by a golden, and a reader of the previous release meets them
//! as unknown fields while this reader reads the previous release's bytes with them unset.

mod common;

use js_proto::FILE_DESCRIPTOR_SET;
use js_proto::v1::{
    Job, JobEvent, JobEventKind, JobSpec, JobState, JobTimes, QueueStatus, QueueStatusCode,
    StartFailureCode,
};
use prost::Message;
use prost_types::FileDescriptorSet;

/// The bytes of a golden `.hex` file: hex pairs separated by white space, each line's text
/// from `#` on a comment.
fn golden_bytes(text: &str) -> Vec<u8> {
    text.lines()
        .map(|line| line.split('#').next().unwrap_or_default())
        .flat_map(str::split_whitespace)
        .map(|pair| u8::from_str_radix(pair, 16).expect("a hex pair"))
        .collect()
}

/// The previous release's `Job`: the fields up to the client. A fixture standing for readers
/// already deployed; it must not follow later changes.
#[derive(Clone, PartialEq, Message)]
struct PreviousJob {
    #[prost(string, tag = "1")]
    id: String,
    #[prost(message, optional, tag = "2")]
    spec: Option<JobSpec>,
    #[prost(enumeration = "JobState", tag = "3")]
    state: i32,
    #[prost(enumeration = "StartFailureCode", tag = "4")]
    start_failure: i32,
    #[prost(string, tag = "5")]
    start_failure_detail: String,
    #[prost(string, tag = "10")]
    host_id: String,
    #[prost(message, optional, tag = "20")]
    times: Option<JobTimes>,
    #[prost(uint64, optional, tag = "30")]
    host_silent_since_ms: Option<u64>,
    #[prost(bool, tag = "31")]
    cancel_requested: bool,
    #[prost(string, optional, tag = "32")]
    client: Option<String>,
}

/// The golden rows of the fields and values the queue status adds, as the golden writes them.
const ADDED_ROWS: [&str; 26] = [
    "Job.queue_status = 33 : QueueStatus",
    "Job.attempts = 34 : uint32",
    "Job.events = 35 : repeated JobEvent",
    "Job.dropped_events = 36 : uint32",
    "QueueStatusCode.QUEUE_STATUS_CODE_UNSPECIFIED = 0",
    "QueueStatusCode.QUEUE_STATUS_CODE_WAITING_FOR_CAPACITY = 1",
    "QueueStatusCode.QUEUE_STATUS_CODE_WAITING_FOR_WHOLE_HOST = 2",
    "QueueStatusCode.QUEUE_STATUS_CODE_QUOTA_BACKOFF = 3",
    "QueueStatusCode.QUEUE_STATUS_CODE_FAILING_OVER = 4",
    "QueueStatusCode.QUEUE_STATUS_CODE_PLACED = 5",
    "QueueStatusCode.QUEUE_STATUS_CODE_WAITING_FOR_MACHINE = 6",
    "QueueStatus.code = 1 : QueueStatusCode",
    "QueueStatus.detail = 2 : string",
    "QueueStatus.since_ms = 3 : uint64",
    "JobEventKind.JOB_EVENT_KIND_UNSPECIFIED = 0",
    "JobEventKind.JOB_EVENT_KIND_WAITING_FOR_CAPACITY = 1",
    "JobEventKind.JOB_EVENT_KIND_WAITING_FOR_WHOLE_HOST = 2",
    "JobEventKind.JOB_EVENT_KIND_QUOTA_BACKOFF = 3",
    "JobEventKind.JOB_EVENT_KIND_FAILING_OVER = 4",
    "JobEventKind.JOB_EVENT_KIND_PLACED = 5",
    "JobEventKind.JOB_EVENT_KIND_REQUEUED = 6",
    "JobEventKind.JOB_EVENT_KIND_WAITING_FOR_MACHINE = 7",
    "JobEvent.at_ms = 1 : uint64",
    "JobEvent.kind = 2 : JobEventKind",
    "JobEvent.detail = 3 : string",
    "JobEvent.host_id = 4 : optional string",
];

/// `common::job()` as the previous release told it.
fn previous() -> PreviousJob {
    let job = common::job();
    PreviousJob {
        id: job.id,
        spec: job.spec,
        state: job.state,
        start_failure: job.start_failure,
        start_failure_detail: job.start_failure_detail,
        host_id: job.host_id,
        times: job.times,
        host_silent_since_ms: job.host_silent_since_ms,
        cancel_requested: job.cancel_requested,
        client: job.client,
    }
}

/// `common::job()` with every queue status field set, as `golden/job_queue_status.hex` pins.
fn queued_job() -> Job {
    Job {
        queue_status: Some(QueueStatus {
            code: QueueStatusCode::QuotaBackoff.into(),
            detail: "q".to_owned(),
            since_ms: 7,
        }),
        attempts: 2,
        events: vec![
            JobEvent {
                at_ms: 1,
                kind: JobEventKind::Placed.into(),
                detail: "p".to_owned(),
                host_id: Some("h".to_owned()),
            },
            JobEvent {
                at_ms: 2,
                kind: JobEventKind::FailingOver.into(),
                detail: String::new(),
                host_id: None,
            },
        ],
        dropped_events: 1,
        ..common::job()
    }
}

/// The numbers of the fields of `message`, or the values of `enumeration`, in the descriptor.
fn numbers(set: &FileDescriptorSet, name: &str) -> Vec<(String, i32)> {
    let files = set.file.iter();
    let message = files.clone().flat_map(|file| &file.message_type);
    let found = message.clone().find(|message| message.name() == name);
    if let Some(message) = found {
        return message
            .field
            .iter()
            .map(|field| (field.name().to_owned(), field.number()))
            .collect();
    }
    let enumeration = files.flat_map(|file| &file.enum_type);
    let found = enumeration.clone().find(|each| each.name() == name);
    let found = found.expect("the type is in the schema");
    found
        .value
        .iter()
        .map(|value| (value.name().to_owned(), value.number()))
        .collect()
}

/// Asserts that the queue status fields are additions in the schema: the golden holds each
/// row once, `Job` numbers them 33 to 36 after the shipped fields with no number used twice,
/// and the two enums number their values in order from an unspecified zero. Catches a
/// renumbered or reused field number and a value renumbered or inserted before another.
#[test]
fn the_queue_status_fields_are_numbered_after_the_shipped_ones() {
    let golden = include_str!("golden/fields.txt");
    for row in ADDED_ROWS {
        let held = golden.lines().filter(|line| *line == row).count();
        assert_eq!(held, 1, "{row}");
    }
    let set = FileDescriptorSet::decode(FILE_DESCRIPTOR_SET).expect("the descriptor set decodes");
    let job = numbers(&set, "Job");
    let mut used: Vec<i32> = job.iter().map(|(_, number)| *number).collect();
    used.sort_unstable();
    used.dedup();
    assert_eq!(used.len(), job.len(), "a number is used twice: {job:?}");
    let added: Vec<(&str, i32)> = job
        .iter()
        .filter(|(_, number)| *number > 32)
        .map(|(name, number)| (name.as_str(), *number))
        .collect();
    assert_eq!(
        added,
        [
            ("queue_status", 33),
            ("attempts", 34),
            ("events", 35),
            ("dropped_events", 36)
        ]
    );
    for name in ["QueueStatusCode", "JobEventKind"] {
        let values: Vec<i32> = numbers(&set, name).into_iter().map(|(_, n)| n).collect();
        let counted: Vec<i32> = (0..).take(values.len()).collect();
        assert_eq!(values, counted, "{name}");
    }
}

/// Asserts that the queue status fields are additions on the wire: a job with them unset
/// encodes byte for byte as the previous release's (golden/job.hex) and decodes in its
/// reader; the previous release's bytes decode here with them unset; a job with them set
/// encodes as the golden's bytes followed by golden/job_queue_status.hex, decodes back as
/// written, and decodes in the previous reader with every old field as sent. Catches a
/// default that is written on the wire, a change of the fields' wire shape, and a previous
/// reader that cannot read this release's answer.
#[test]
fn the_queue_status_fields_are_additive_on_the_wire() {
    let old_bytes = golden_bytes(include_str!("golden/job.hex"));
    assert_eq!(previous().encode_to_vec(), old_bytes);
    assert_eq!(common::job().encode_to_vec(), old_bytes);
    let read = Job::decode(old_bytes.as_slice()).expect("this reader decodes the old bytes");
    assert_eq!(read.queue_status, None);
    assert_eq!((read.attempts, read.dropped_events), (0, 0));
    assert!(read.events.is_empty());
    assert_eq!(read, common::job());

    let mut bytes = old_bytes.clone();
    bytes.extend(golden_bytes(include_str!("golden/job_queue_status.hex")));
    assert_eq!(queued_job().encode_to_vec(), bytes);
    assert_eq!(
        Job::decode(bytes.as_slice()).expect("decodes"),
        queued_job()
    );
    let old_reader = PreviousJob::decode(bytes.as_slice()).expect("the previous reader decodes");
    assert_eq!(old_reader, previous());
}

/// Asserts that a code or kind a later release appends reads, through the typed accessors, as
/// unspecified, and the rest of the job as sent. Catches a reader that refuses a job for a
/// value it does not know.
#[test]
fn a_later_code_or_kind_reads_as_unspecified() {
    let later = Job {
        queue_status: Some(QueueStatus {
            code: 99,
            ..QueueStatus::default()
        }),
        events: vec![JobEvent {
            kind: 99,
            ..JobEvent::default()
        }],
        ..common::job()
    };
    let read = Job::decode(later.encode_to_vec().as_slice()).expect("decodes");
    let status = read.queue_status.expect("kept");
    assert_eq!(status.code(), QueueStatusCode::Unspecified);
    assert_eq!(read.events[0].kind(), JobEventKind::Unspecified);
    assert_eq!(read.id, common::job().id);
}
