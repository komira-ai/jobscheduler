//! A job's retry policy and its attempts (docs/design/job-retry-and-completion-callback.md,
//! sections 3.1, 3.7 and 3.8), held to the evolution rule of the package: every field and
//! value is an addition numbered after the shipped ones, the bytes they add are pinned by
//! goldens, a reader of the previous release meets them as unknown fields, and this reader
//! reads the previous release's bytes with them at their defaults.

mod common;

use js_proto::FILE_DESCRIPTOR_SET;
use js_proto::v1::{
    Attempt, AttemptOutcome, GetLogsRequest, Job, JobEvent, JobEventKind, JobResult, JobSpec,
    JobState, JobTimes, LogStream, QueueStatus, QueueStatusCode, ResultReason, RetryPolicy,
    StartFailureCode, SubmitJobRequest, UsageRecord,
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

/// The golden rows of the fields and values the retry policy and the attempts add, as the
/// golden writes them.
const ADDED_ROWS: [&str; 30] = [
    "JobSpec.retry = 70 : RetryPolicy",
    "RetryPolicy.max_attempts = 1 : uint32",
    "RetryPolicy.initial_backoff_seconds = 2 : optional uint32",
    "RetryPolicy.max_backoff_seconds = 3 : optional uint32",
    "Job.attempt = 37 : uint32",
    "Job.attempt_history = 38 : repeated Attempt",
    "Job.retry_not_before_ms = 39 : optional uint64",
    "Job.dropped_attempts = 41 : uint32",
    "AttemptOutcome.ATTEMPT_OUTCOME_UNSPECIFIED = 0",
    "AttemptOutcome.ATTEMPT_OUTCOME_SUCCEEDED = 1",
    "AttemptOutcome.ATTEMPT_OUTCOME_FAILED = 2",
    "AttemptOutcome.ATTEMPT_OUTCOME_HOST_LOST = 3",
    "AttemptOutcome.ATTEMPT_OUTCOME_START_FAILED = 4",
    "AttemptOutcome.ATTEMPT_OUTCOME_CANCELLED = 5",
    "Attempt.number = 1 : uint32",
    "Attempt.host_id = 2 : string",
    "Attempt.outcome = 3 : AttemptOutcome",
    "Attempt.times = 4 : JobTimes",
    "Attempt.exit_code = 5 : optional int32",
    "Attempt.reason = 6 : ResultReason",
    "Attempt.start_failure = 7 : StartFailureCode",
    "Attempt.start_failure_detail = 8 : string",
    "Attempt.retried = 9 : bool",
    "JobResult.attempt = 13 : uint32",
    "UsageRecord.attempt = 24 : optional uint32",
    "GetLogsRequest.attempt = 6 : optional uint32",
    "QueueStatusCode.QUEUE_STATUS_CODE_WAITING_FOR_RETRY = 7",
    "JobEventKind.JOB_EVENT_KIND_RETRY_SCHEDULED = 8",
    // The two shipped rows whose enums gained a value: the new value is after them.
    "QueueStatusCode.QUEUE_STATUS_CODE_WAITING_FOR_MACHINE = 6",
    "JobEventKind.JOB_EVENT_KIND_WAITING_FOR_MACHINE = 7",
];

/// The previous release's `Job`: the fields up to the dropped events. A fixture standing for
/// readers already deployed; it must not follow later changes.
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
    #[prost(message, optional, tag = "33")]
    queue_status: Option<QueueStatus>,
    #[prost(uint32, tag = "34")]
    attempts: u32,
    #[prost(message, repeated, tag = "35")]
    events: Vec<JobEvent>,
    #[prost(uint32, tag = "36")]
    dropped_events: u32,
}

/// The fields of the previous release's `JobSpec` this test reads: a reader that knows every
/// field up to the mounts and nothing after them reads the same.
#[derive(Clone, PartialEq, Message)]
struct PreviousSpec {
    #[prost(string, tag = "1")]
    job_id: String,
    #[prost(string, tag = "52")]
    history_key: String,
}

/// The previous release's `JobResult`: its fields up to the input failure, the bytes ones
/// standing for the messages and tails.
#[derive(Clone, PartialEq, Message)]
struct PreviousResult {
    #[prost(enumeration = "JobState", tag = "1")]
    state: i32,
    #[prost(int32, optional, tag = "2")]
    exit_code: Option<i32>,
    #[prost(string, optional, tag = "12")]
    input_failure: Option<String>,
}

/// The previous release's `UsageRecord`, the fields this test reads.
#[derive(Clone, PartialEq, Message)]
struct PreviousUsage {
    #[prost(string, tag = "1")]
    job_id: String,
    #[prost(uint64, tag = "7")]
    ended_at_ms: u64,
}

/// The previous release's `GetLogsRequest`: the fields up to follow.
#[derive(Clone, PartialEq, Message)]
struct PreviousLogsRequest {
    #[prost(string, tag = "1")]
    job_id: String,
    #[prost(enumeration = "LogStream", tag = "2")]
    stream: i32,
    #[prost(uint64, optional, tag = "3")]
    from_offset: Option<u64>,
    #[prost(uint32, optional, tag = "4")]
    max_bytes: Option<u32>,
    #[prost(bool, optional, tag = "5")]
    follow: Option<bool>,
}

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
        queue_status: job.queue_status,
        attempts: job.attempts,
        events: job.events,
        dropped_events: job.dropped_events,
    }
}

/// The times of an attempt, from `queued_at_ms` with the later ones as given.
fn times(queued: u64, placed: Option<u64>, started: Option<u64>, ended: Option<u64>) -> JobTimes {
    JobTimes {
        queued_at_ms: queued,
        placed_at_ms: placed,
        started_at_ms: started,
        ended_at_ms: ended,
    }
}

/// `common::job()` with every attempt field set, as `golden/job_retry.hex` pins: three ended
/// attempts, the first a start failure and the second a lost host, both retried, and the third
/// a failed run that was not.
fn retried_job() -> Job {
    Job {
        attempt: 3,
        attempt_history: vec![
            Attempt {
                number: 1,
                host_id: "h".to_owned(),
                outcome: AttemptOutcome::StartFailed.into(),
                times: Some(times(5, Some(6), None, None)),
                exit_code: None,
                reason: ResultReason::Unspecified.into(),
                start_failure: StartFailureCode::AgentRefused.into(),
                start_failure_detail: "host-tainted".to_owned(),
                retried: true,
            },
            Attempt {
                number: 2,
                host_id: "g".to_owned(),
                outcome: AttemptOutcome::HostLost.into(),
                times: Some(times(8, Some(9), Some(10), None)),
                retried: true,
                ..Attempt::default()
            },
            Attempt {
                number: 3,
                host_id: "h".to_owned(),
                outcome: AttemptOutcome::Failed.into(),
                times: Some(times(11, Some(12), Some(13), Some(14))),
                exit_code: Some(2),
                reason: ResultReason::Exit.into(),
                retried: false,
                ..Attempt::default()
            },
        ],
        retry_not_before_ms: Some(15),
        dropped_attempts: 1,
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

/// The fields of `message` numbered at or above `from`, by name and number.
fn added(set: &FileDescriptorSet, message: &str, from: i32) -> Vec<(String, i32)> {
    numbers(set, message)
        .into_iter()
        .filter(|(_, number)| *number >= from)
        .collect()
}

/// Owned pairs of the names and numbers given.
fn pairs(given: &[(&str, i32)]) -> Vec<(String, i32)> {
    given
        .iter()
        .map(|(name, number)| ((*name).to_owned(), *number))
        .collect()
}

/// Asserts that the retry and attempt fields are additions in the schema: the field golden
/// holds each row once; `JobSpec` numbers `retry` 70, after the shipped fields; `Job` numbers
/// its attempt fields 37 to 41 with no number used twice; `JobResult`, `UsageRecord` and
/// `GetLogsRequest` number theirs 13, 24 and 6; the two enums of outcomes and states count from
/// an unspecified zero, and `QueueStatusCode` and `JobEventKind` gained their values at their
/// ends. Catches a renumbered or reused field number, a value inserted before a shipped one,
/// and a field the golden does not ship.
#[test]
fn the_retry_fields_are_numbered_after_the_shipped_ones() {
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
    assert_eq!(
        added(&set, "Job", 37),
        pairs(&[
            ("attempt", 37),
            ("attempt_history", 38),
            ("retry_not_before_ms", 39),
            ("callback_status", 40),
            ("dropped_attempts", 41),
        ])
    );
    assert_eq!(
        added(&set, "JobSpec", 62),
        pairs(&[("retry", 70), ("callback", 71)])
    );
    assert_eq!(added(&set, "JobResult", 13), pairs(&[("attempt", 13)]));
    assert_eq!(added(&set, "UsageRecord", 24), pairs(&[("attempt", 24)]));
    assert_eq!(added(&set, "GetLogsRequest", 6), pairs(&[("attempt", 6)]));
    for name in [
        "AttemptOutcome",
        "QueueStatusCode",
        "JobEventKind",
        "Attempt",
        "RetryPolicy",
    ] {
        let values: Vec<i32> = numbers(&set, name).into_iter().map(|(_, n)| n).collect();
        let first = i32::from(name == "Attempt" || name == "RetryPolicy");
        let counted: Vec<i32> = (first..).take(values.len()).collect();
        assert_eq!(values, counted, "{name}");
    }
    let queue = numbers(&set, "QueueStatusCode");
    assert_eq!(
        queue.last(),
        Some(&("QUEUE_STATUS_CODE_WAITING_FOR_RETRY".to_owned(), 7))
    );
    let events = numbers(&set, "JobEventKind");
    assert_eq!(
        events.last(),
        Some(&("JOB_EVENT_KIND_RETRY_SCHEDULED".to_owned(), 8))
    );
}

/// Asserts that the attempt fields of `Job` are additions on the wire: the previous release's
/// bytes (golden/job.hex) decode here with attempt 0, no history, no not-before and nothing
/// dropped; a job with every attempt field set encodes as those bytes followed by
/// golden/job_retry.hex, decodes back as written, and decodes in the previous reader with
/// every old field as sent. Catches a default that is written on the wire, a change of the
/// fields' wire shape (a retyped exit code, a lost presence), and a previous reader that
/// cannot read this release's answer.
#[test]
fn the_attempt_fields_are_additive_on_the_wire() {
    let old_bytes = golden_bytes(include_str!("golden/job.hex"));
    assert_eq!(previous().encode_to_vec(), old_bytes);
    let read = Job::decode(old_bytes.as_slice()).expect("this reader decodes the old bytes");
    assert_eq!((read.attempt, read.dropped_attempts), (0, 0));
    assert_eq!(read.retry_not_before_ms, None);
    assert!(read.attempt_history.is_empty());
    assert_eq!(read, common::job());

    let mut bytes = old_bytes.clone();
    bytes.extend(golden_bytes(include_str!("golden/job_retry.hex")));
    assert_eq!(retried_job().encode_to_vec(), bytes);
    assert_eq!(
        Job::decode(bytes.as_slice()).expect("decodes"),
        retried_job()
    );
    let old_reader = PreviousJob::decode(bytes.as_slice()).expect("the previous reader decodes");
    assert_eq!(old_reader, previous());
}

/// Asserts that `JobSpec.retry` is an addition on the wire: a spec without it encodes as the
/// spec of golden/submit_job_request.hex; with a policy of three attempts, a present initial
/// wait of 0 and a cap of 600 seconds it encodes as those bytes followed by
/// golden/spec_retry.hex and decodes back as written; a previous reader reads the old fields
/// as sent; a policy at its defaults writes nothing of its own and reads back with both waits
/// absent, and a spec that carries it writes it as an empty message. Catches a policy written
/// when absent, a wait that loses its presence (so absent, which means the default, would read
/// as 0), and a renumbered policy.
#[test]
fn the_retry_policy_is_additive_on_the_wire() {
    let request = golden_bytes(include_str!("golden/submit_job_request.hex"));
    let request = SubmitJobRequest::decode(request.as_slice()).expect("the golden decodes");
    let spec = request.spec.expect("the golden has a spec");
    assert_eq!(spec, common::spec("presigned-url-text"));
    assert_eq!(spec.retry, None);
    let old_bytes = spec.encode_to_vec();

    let asked = JobSpec {
        retry: Some(RetryPolicy {
            max_attempts: 3,
            initial_backoff_seconds: Some(0),
            max_backoff_seconds: Some(600),
        }),
        ..spec.clone()
    };
    let mut bytes = old_bytes.clone();
    bytes.extend(golden_bytes(include_str!("golden/spec_retry.hex")));
    assert_eq!(asked.encode_to_vec(), bytes);
    assert_eq!(JobSpec::decode(bytes.as_slice()), Ok(asked));
    let old_reader = PreviousSpec::decode(bytes.as_slice()).expect("the previous reader decodes");
    assert_eq!(
        old_reader,
        PreviousSpec::decode(old_bytes.as_slice()).expect("decodes")
    );
    assert_eq!(old_reader.job_id, "job-1");

    let defaults = RetryPolicy {
        max_attempts: 0,
        initial_backoff_seconds: None,
        max_backoff_seconds: None,
    };
    let written = defaults.encode_to_vec();
    assert!(written.is_empty(), "{written:?}");
    let read = RetryPolicy::decode(written.as_slice()).expect("decodes");
    assert_eq!(read, defaults);
    let present = JobSpec {
        retry: Some(defaults),
        ..JobSpec::default()
    };
    assert_eq!(present.encode_to_vec(), [0xb2, 0x04, 0x00]);
}

/// Asserts that the attempt numbers of a result, a usage record and a logs request are
/// additions on the wire: each previous message's bytes decode with no attempt (0 for the
/// result, absent for the other two), each attempt is written under its pinned bytes (result
/// 13, `0x68`; usage 24, `0xc0 0x01`, present when 0; logs 6, `0x30`), and a previous reader
/// skips them. Catches a renumbered attempt, a usage or logs attempt that loses its presence
/// (so absent, which means 1 or the latest, would read as 0), and a reader of the previous
/// release that refuses the new bytes.
#[test]
fn the_attempt_of_a_result_a_usage_record_and_a_logs_request_is_additive() {
    let result = JobResult {
        state: JobState::Failed.into(),
        exit_code: Some(1),
        ..JobResult::default()
    };
    let old_bytes = result.encode_to_vec();
    assert_eq!(old_bytes, [0x08, 0x06, 0x10, 0x01]);
    assert_eq!(JobResult::decode(old_bytes.as_slice()), Ok(result.clone()));
    let attempted = JobResult {
        attempt: 2,
        ..result
    };
    let bytes = attempted.encode_to_vec();
    assert_eq!(bytes, [0x08, 0x06, 0x10, 0x01, 0x68, 0x02]);
    assert_eq!(JobResult::decode(bytes.as_slice()), Ok(attempted));
    assert_eq!(
        PreviousResult::decode(bytes.as_slice()).expect("the previous reader decodes"),
        PreviousResult {
            state: JobState::Failed.into(),
            exit_code: Some(1),
            input_failure: None,
        }
    );

    let usage = UsageRecord {
        job_id: "j".to_owned(),
        ..UsageRecord::default()
    };
    let old_bytes = usage.encode_to_vec();
    assert_eq!(old_bytes, [0x0a, 0x01, b'j']);
    let read = UsageRecord::decode(old_bytes.as_slice()).expect("decodes");
    assert_eq!(read.attempt, None);
    let attempted = UsageRecord {
        attempt: Some(0),
        ..usage
    };
    let bytes = attempted.encode_to_vec();
    assert_eq!(bytes, [0x0a, 0x01, b'j', 0xc0, 0x01, 0x00]);
    assert_eq!(UsageRecord::decode(bytes.as_slice()), Ok(attempted));
    assert_eq!(
        PreviousUsage::decode(bytes.as_slice()).expect("the previous reader decodes"),
        PreviousUsage {
            job_id: "j".to_owned(),
            ended_at_ms: 0,
        }
    );

    let logs = GetLogsRequest {
        job_id: "j".to_owned(),
        stream: LogStream::Stderr.into(),
        ..GetLogsRequest::default()
    };
    let old_bytes = logs.encode_to_vec();
    assert_eq!(old_bytes, [0x0a, 0x01, b'j', 0x10, 0x02]);
    let read = GetLogsRequest::decode(old_bytes.as_slice()).expect("decodes");
    assert_eq!(read.attempt, None);
    let attempted = GetLogsRequest {
        attempt: Some(2),
        ..logs
    };
    let bytes = attempted.encode_to_vec();
    assert_eq!(bytes, [0x0a, 0x01, b'j', 0x10, 0x02, 0x30, 0x02]);
    assert_eq!(GetLogsRequest::decode(bytes.as_slice()), Ok(attempted));
    assert_eq!(
        PreviousLogsRequest::decode(bytes.as_slice()).expect("the previous reader decodes"),
        PreviousLogsRequest {
            job_id: "j".to_owned(),
            stream: LogStream::Stderr.into(),
            ..PreviousLogsRequest::default()
        }
    );
}

/// Asserts that an attempt outcome a later release appends reads, through the typed
/// accessor, as unspecified, and that the new queue status code and event kind keep their
/// numbers and read as themselves here. Catches a reader that refuses a job for a value it
/// does not know, and a value that reads as another.
#[test]
fn a_later_outcome_reads_as_unspecified_and_the_new_values_read_as_themselves() {
    let later = Job {
        attempt_history: vec![Attempt {
            outcome: 99,
            ..Attempt::default()
        }],
        queue_status: Some(QueueStatus {
            code: QueueStatusCode::WaitingForRetry.into(),
            ..QueueStatus::default()
        }),
        events: vec![JobEvent {
            kind: JobEventKind::RetryScheduled.into(),
            ..JobEvent::default()
        }],
        ..common::job()
    };
    let read = Job::decode(later.encode_to_vec().as_slice()).expect("decodes");
    assert_eq!(
        read.attempt_history[0].outcome(),
        AttemptOutcome::Unspecified
    );
    assert_eq!(read.attempt_history[0].outcome, 99);
    let status = read.queue_status.expect("kept");
    assert_eq!(status.code, 7);
    assert_eq!(status.code(), QueueStatusCode::WaitingForRetry);
    assert_eq!(read.events[0].kind, 8);
    assert_eq!(read.events[0].kind(), JobEventKind::RetryScheduled);
}
