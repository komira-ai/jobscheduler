//! A job's completion callback and its delivery status
//! (docs/design/job-retry-and-completion-callback.md, sections 5.2 and 5.5), held to the
//! evolution rule of the package: every field and value is an addition numbered after the
//! shipped ones, the bytes they add are pinned by goldens, a reader of the previous release
//! meets them as unknown fields, and this reader reads the previous release's bytes with them
//! absent.

mod common;

use js_proto::FILE_DESCRIPTOR_SET;
use js_proto::v1::{Callback, CallbackState, CallbackStatus, Job, JobSpec, SubmitJobRequest};
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

/// The golden rows of the fields and values the callback adds, as the golden writes them.
const ADDED_ROWS: [&str; 17] = [
    "JobSpec.callback = 71 : Callback",
    "Callback.url = 1 : string",
    "Callback.signing_key = 2 : bytes",
    "Callback.deadline_seconds = 3 : optional uint32",
    "Job.callback_status = 40 : CallbackStatus",
    "CallbackState.CALLBACK_STATE_UNSPECIFIED = 0",
    "CallbackState.CALLBACK_STATE_ARMED = 1",
    "CallbackState.CALLBACK_STATE_PENDING = 2",
    "CallbackState.CALLBACK_STATE_DELIVERED = 3",
    "CallbackState.CALLBACK_STATE_EXPIRED = 4",
    "CallbackState.CALLBACK_STATE_ABANDONED = 5",
    "CallbackStatus.state = 1 : CallbackState",
    "CallbackStatus.tries = 2 : uint32",
    "CallbackStatus.last_try_at_ms = 3 : optional uint64",
    "CallbackStatus.last_failure = 4 : string",
    "CallbackStatus.delivered_at_ms = 5 : optional uint64",
    "CallbackStatus.deadline_at_ms = 6 : optional uint64",
];

/// The fields of the previous release's `Job` this test reads: a reader that knows every
/// field up to the dropped events and nothing after them reads the same.
#[derive(Clone, PartialEq, Message)]
struct PreviousJob {
    #[prost(string, tag = "1")]
    id: String,
    #[prost(string, tag = "10")]
    host_id: String,
    #[prost(uint32, tag = "36")]
    dropped_events: u32,
}

/// The fields of the previous release's `JobSpec` this test reads.
#[derive(Clone, PartialEq, Message)]
struct PreviousSpec {
    #[prost(string, tag = "1")]
    job_id: String,
    #[prost(string, tag = "52")]
    history_key: String,
}

/// The callback `golden/spec_callback.hex` pins: a URL, the 32 bytes 0x20 to 0x3f as its key,
/// and a present deadline of 0.
fn callback() -> Callback {
    Callback {
        url: "https://h.example/r".to_owned(),
        signing_key: (0x20..0x40).collect(),
        deadline_seconds: Some(0),
    }
}

/// `common::job()` with its callback status set, as `golden/job_callback.hex` pins.
fn called_back_job() -> Job {
    Job {
        callback_status: Some(CallbackStatus {
            state: CallbackState::Pending.into(),
            tries: 3,
            last_try_at_ms: Some(4),
            last_failure: "failed".to_owned(),
            delivered_at_ms: Some(0),
            deadline_at_ms: Some(6),
        }),
        ..common::job()
    }
}

/// The numbers of the fields of `message`, or the values of `enumeration`, in the descriptor.
fn numbers(set: &FileDescriptorSet, name: &str) -> Vec<i32> {
    let files = set.file.iter();
    let message = files.clone().flat_map(|file| &file.message_type);
    if let Some(message) = message.clone().find(|message| message.name() == name) {
        return message.field.iter().map(|field| field.number()).collect();
    }
    let enumeration = files.flat_map(|file| &file.enum_type);
    let found = enumeration.clone().find(|each| each.name() == name);
    let found = found.expect("the type is in the schema");
    found.value.iter().map(|value| value.number()).collect()
}

/// Asserts that the callback's fields are additions in the schema: the field golden holds
/// each row once, `Callback` and `CallbackStatus` number their fields from 1 in order, and
/// `CallbackState` numbers its values from an unspecified zero in order. Catches a field the
/// golden does not ship, and a value inserted before another.
#[test]
fn the_callback_fields_are_in_the_golden_and_numbered_in_order() {
    let golden = include_str!("golden/fields.txt");
    for row in ADDED_ROWS {
        let held = golden.lines().filter(|line| *line == row).count();
        assert_eq!(held, 1, "{row}");
    }
    let set = FileDescriptorSet::decode(FILE_DESCRIPTOR_SET).expect("the descriptor set decodes");
    assert_eq!(numbers(&set, "Callback"), [1, 2, 3]);
    assert_eq!(numbers(&set, "CallbackStatus"), [1, 2, 3, 4, 5, 6]);
    assert_eq!(numbers(&set, "CallbackState"), [0, 1, 2, 3, 4, 5]);
}

/// Asserts that `JobSpec.callback` is an addition on the wire: the spec of
/// golden/submit_job_request.hex has none; with one it encodes as the spec's bytes followed by
/// golden/spec_callback.hex (the URL, the key as bytes and a present deadline of 0) and
/// decodes back as written; a previous reader reads the old fields as sent; and a callback
/// with no deadline writes none, so absent (86400 seconds) never reads as 0. Catches a
/// callback written when absent, a key that is not carried as raw bytes, a deadline that
/// loses its presence, and a renumbered callback.
#[test]
fn the_callback_is_additive_on_the_wire() {
    let request = golden_bytes(include_str!("golden/submit_job_request.hex"));
    let request = SubmitJobRequest::decode(request.as_slice()).expect("the golden decodes");
    let spec = request.spec.expect("the golden has a spec");
    assert_eq!(spec.callback, None);
    let old_bytes = spec.encode_to_vec();

    let asked = JobSpec {
        callback: Some(callback()),
        ..spec
    };
    let mut bytes = old_bytes.clone();
    bytes.extend(golden_bytes(include_str!("golden/spec_callback.hex")));
    assert_eq!(asked.encode_to_vec(), bytes);
    assert_eq!(JobSpec::decode(bytes.as_slice()), Ok(asked));
    let old_reader = PreviousSpec::decode(bytes.as_slice()).expect("the previous reader decodes");
    assert_eq!(
        old_reader,
        PreviousSpec::decode(old_bytes.as_slice()).expect("decodes")
    );

    let no_deadline = Callback {
        deadline_seconds: None,
        ..callback()
    };
    let read = Callback::decode(no_deadline.encode_to_vec().as_slice()).expect("decodes");
    assert_eq!(read.deadline_seconds, None);
    assert_eq!(read, no_deadline);
}

/// Asserts that `Job.callback_status` is an addition on the wire: the previous release's
/// bytes (golden/job.hex) decode here with no status, which means the job has no callback; a
/// job with a status encodes as those bytes followed by golden/job_callback.hex and decodes
/// back as written; and a previous reader reads the old fields as sent. Catches a status
/// written when absent, a delivered time that loses its presence, and a reader of the
/// previous release that refuses this release's answer.
#[test]
fn the_callback_status_is_additive_on_the_wire() {
    let old_bytes = golden_bytes(include_str!("golden/job.hex"));
    let read = Job::decode(old_bytes.as_slice()).expect("this reader decodes the old bytes");
    assert_eq!(read.callback_status, None);
    assert_eq!(read, common::job());

    let mut bytes = old_bytes.clone();
    bytes.extend(golden_bytes(include_str!("golden/job_callback.hex")));
    assert_eq!(called_back_job().encode_to_vec(), bytes);
    assert_eq!(
        Job::decode(bytes.as_slice()).expect("decodes"),
        called_back_job()
    );
    assert_eq!(
        PreviousJob::decode(bytes.as_slice()).expect("the previous reader decodes"),
        PreviousJob::decode(old_bytes.as_slice()).expect("decodes")
    );
}

/// Asserts that a callback state a later release appends reads, through the typed accessor,
/// as unspecified, with its number kept. Catches a reader that refuses a job for a state it
/// does not know.
#[test]
fn a_later_callback_state_reads_as_unspecified() {
    let later = Job {
        callback_status: Some(CallbackStatus {
            state: 99,
            ..CallbackStatus::default()
        }),
        ..common::job()
    };
    let read = Job::decode(later.encode_to_vec().as_slice()).expect("decodes");
    let status = read.callback_status.expect("kept");
    assert_eq!(status.state(), CallbackState::Unspecified);
    assert_eq!(status.state, 99);
}
