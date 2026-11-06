//! The wire format of the messages: fully populated messages against hand-reviewable
//! golden bytes, decoding across schema versions (unknown fields of every wire type, at the
//! top and nested, unknown enum values, a later reader of this version's bytes), values at
//! the boundaries of their types, and the defaults a submit gets for what it leaves out.

mod common;

use js_proto::FILE_DESCRIPTOR_SET;
use js_proto::refusal::{refusal_of, refusal_status};
use js_proto::v1::{
    GetLogsResponse, Job, JobResult, JobSpec, JobState, JobTimes, ListJobsRequest, NetworkLevel,
    Refusal, RefusalCode, ResourceUsage, StartFailureCode, SubmitJobRequest,
};
use prost::Message;
use prost::encoding::{WireType, encode_key, encode_varint};
use prost_types::FileDescriptorSet;

/// The bytes of a golden `.hex` file: hex pairs separated by white space, each line's text
/// from `#` on a comment.
fn golden_bytes(text: &str) -> Vec<u8> {
    text.lines()
        .map(|line| line.split('#').next().unwrap_or_default())
        .flat_map(str::split_whitespace)
        .map(|pair| {
            assert_eq!(pair.len(), 2, "{pair:?} is not one byte in hex");
            u8::from_str_radix(pair, 16).expect("a hex pair")
        })
        .collect()
}

fn submit_golden() -> Vec<u8> {
    golden_bytes(include_str!("golden/submit_job_request.hex"))
}

fn job_golden() -> Vec<u8> {
    golden_bytes(include_str!("golden/job.hex"))
}

/// `bytes` with four fields appended that no message of this version has, one of each wire
/// type a new field can have (varint, length-delimited, 32-bit and 64-bit), numbered far
/// above any number in use.
fn with_unknown_fields(bytes: &[u8]) -> Vec<u8> {
    let mut out = bytes.to_vec();
    encode_key(100_000, WireType::Varint, &mut out);
    encode_varint(42, &mut out);
    encode_key(100_001, WireType::LengthDelimited, &mut out);
    encode_varint(3, &mut out);
    out.extend_from_slice(b"new");
    encode_key(100_002, WireType::ThirtyTwoBit, &mut out);
    out.extend_from_slice(&7_u32.to_le_bytes());
    encode_key(100_003, WireType::SixtyFourBit, &mut out);
    out.extend_from_slice(&u64::MAX.to_le_bytes());
    out
}

/// `Job` as a later version could define it: every field of this version at its number and
/// type, and two fields added after it (a plain string and an optional integer).
#[derive(Clone, PartialEq, Message)]
struct LaterJob {
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
    #[prost(string, tag = "40")]
    platform_handle: String,
    #[prost(uint64, optional, tag = "41")]
    requeued_at_ms: Option<u64>,
}

/// The leading comment of the element at `path` in `file` of the descriptor set.
fn comment(file: &str, path: &[i32]) -> String {
    let set = FileDescriptorSet::decode(FILE_DESCRIPTOR_SET).expect("the descriptor set decodes");
    let file = set
        .file
        .iter()
        .find(|f| f.name() == file)
        .expect("the file is in the set");
    file.source_code_info
        .as_ref()
        .expect("the set keeps the comments")
        .location
        .iter()
        .find(|l| l.path == path)
        .map(|l| l.leading_comments().to_owned())
        .expect("the element has a location")
}

/// Asserts that a fully populated `SubmitJobRequest` and a fully populated `Job` encode to
/// the golden bytes, with each map's two keys in key order though they were inserted in
/// reverse, and decode back equal. Catches a changed field number, wire type or map order.
#[test]
fn request_and_job_encode_to_golden_bytes() {
    let request = SubmitJobRequest {
        spec: Some(common::spec("presigned-url-text")),
    };
    assert_eq!(request.encode_to_vec(), submit_golden());
    assert_eq!(
        SubmitJobRequest::decode(submit_golden().as_slice()).expect("the golden decodes"),
        request
    );
    let job = common::job();
    assert_eq!(job.encode_to_vec(), job_golden());
    assert_eq!(
        Job::decode(job_golden().as_slice()).expect("the golden decodes"),
        job
    );
}

/// Asserts that the golden bytes with a varint and a length-delimited field of a newer
/// schema appended decode to the same messages (a reader of this version takes data from a
/// newer writer), and that the `Job` golden, which holds only the fields of this version,
/// decodes with every other field at its default: encoding the decoded job again gives
/// back exactly the golden, which a field holding anything but its default would change
/// (a newer reader takes data from this version).
#[test]
fn bytes_from_a_newer_schema_decode() {
    let newer = with_unknown_fields(&submit_golden());
    assert_ne!(newer, submit_golden());
    assert_eq!(
        SubmitJobRequest::decode(newer.as_slice()).expect("newer bytes decode"),
        SubmitJobRequest {
            spec: Some(common::spec("presigned-url-text")),
        }
    );
    let newer = with_unknown_fields(&job_golden());
    assert_eq!(
        Job::decode(newer.as_slice()).expect("newer bytes decode"),
        common::job()
    );
    let decoded = Job::decode(job_golden().as_slice()).expect("the golden decodes");
    assert_eq!(decoded.encode_to_vec(), job_golden());
}

/// Asserts that a `JobSpec` left at its defaults encodes to nothing, decodes with the
/// network level unspecified and no ports, and that the protobuf documents unspecified as
/// none, both on the value and on the field. Catches a default network level other than
/// none, or a port added by default.
#[test]
fn submit_defaults_are_no_network_and_no_ports() {
    let spec = JobSpec::default();
    assert_eq!(spec.encode_to_vec(), Vec::<u8>::new());
    let decoded = JobSpec::decode(&b""[..]).expect("empty bytes decode");
    assert_eq!(decoded.network(), NetworkLevel::Unspecified);
    assert_eq!(decoded.network, 0);
    assert!(decoded.ports.is_empty());
    assert_eq!(
        NetworkLevel::Unspecified.as_str_name(),
        "NETWORK_LEVEL_UNSPECIFIED"
    );
    // job.proto: enum_type 1 (NetworkLevel), value 0; message_type 4 (JobSpec), field 8
    // (network). Descriptor paths: 5 is enum_type, 2 its value; 4 is message_type, 2 its
    // field.
    assert_eq!(
        comment("jobscheduler/v1/job.proto", &[5, 1, 2, 0]),
        " Not given: read as none, the default.\n"
    );
    assert_eq!(
        comment("jobscheduler/v1/job.proto", &[4, 4, 2, 8]),
        " What the job may reach over the network. Unspecified: none.\n"
    );
}

/// Asserts that a spec without a wall limit, one with 0 and one with 1 second encode to
/// three different byte strings and each decodes back to its own value, so a server can
/// tell a missing limit from a given one and refuse it. Catches the field losing its
/// presence (a plain `uint64` would encode the first two alike).
#[test]
fn wall_limit_zero_is_distinguishable_from_a_value() {
    let specs = [None, Some(0), Some(1)].map(|wall_limit_seconds| JobSpec {
        wall_limit_seconds,
        ..JobSpec::default()
    });
    let encoded = specs.clone().map(|s| s.encode_to_vec());
    assert_eq!(encoded[0], Vec::<u8>::new());
    assert_eq!(encoded[1], [0xa0, 0x01, 0x00]);
    assert_eq!(encoded[2], [0xa0, 0x01, 0x01]);
    for (bytes, spec) in encoded.iter().zip(&specs) {
        assert_eq!(&JobSpec::decode(bytes.as_slice()).expect("decodes"), spec);
    }
}

/// Asserts that the `Job` golden, written by this version, decodes with a later reader whose
/// `Job` has two more fields: every field of this version comes back as written and both
/// added fields are absent (an empty string, `None`). And the reverse: a later writer's job
/// with both added fields set decodes with this version to the job of this version.
#[test]
fn a_later_reader_reads_this_version_and_this_reader_reads_a_later_one() {
    let LaterJob {
        id,
        spec,
        state,
        start_failure,
        start_failure_detail,
        host_id,
        times,
        host_silent_since_ms,
        cancel_requested,
        platform_handle,
        requeued_at_ms,
    } = LaterJob::decode(job_golden().as_slice()).expect("the golden decodes");
    let job = common::job();
    assert_eq!(
        (id, spec, state, start_failure, start_failure_detail),
        (
            job.id.clone(),
            job.spec.clone(),
            job.state,
            job.start_failure,
            job.start_failure_detail.clone()
        )
    );
    assert_eq!(
        (host_id, times, host_silent_since_ms, cancel_requested),
        (
            job.host_id.clone(),
            job.times,
            job.host_silent_since_ms,
            job.cancel_requested
        )
    );
    assert_eq!((platform_handle, requeued_at_ms), (String::new(), None));

    let later = LaterJob {
        id: job.id.clone(),
        spec: job.spec.clone(),
        state: job.state,
        start_failure: job.start_failure,
        start_failure_detail: job.start_failure_detail.clone(),
        host_id: job.host_id.clone(),
        times: job.times,
        host_silent_since_ms: job.host_silent_since_ms,
        cancel_requested: job.cancel_requested,
        platform_handle: "platform-handle-1".to_owned(),
        requeued_at_ms: Some(0),
    };
    assert_eq!(
        Job::decode(later.encode_to_vec().as_slice()).expect("later bytes decode"),
        job
    );
}

/// Asserts that an enum value this version does not know survives a decode in its raw
/// number, in a plain field and in a repeated one, while the typed accessors read it as the
/// enum's documented fallback: `UNSPECIFIED` for a plain field, and left out of a repeated
/// field's typed list.
#[test]
fn an_unknown_enum_value_keeps_its_number_and_reads_as_unspecified() {
    let spec = JobSpec {
        network: 99,
        ..JobSpec::default()
    };
    let decoded = JobSpec::decode(spec.encode_to_vec().as_slice()).expect("decodes");
    assert_eq!(decoded.network, 99);
    assert_eq!(decoded.network(), NetworkLevel::Unspecified);

    let request = ListJobsRequest {
        states: vec![JobState::Queued.into(), 99, JobState::Running.into()],
        ..ListJobsRequest::default()
    };
    let decoded = ListJobsRequest::decode(request.encode_to_vec().as_slice()).expect("decodes");
    assert_eq!(decoded.states, [1, 99, 4]);
    assert_eq!(
        decoded.states().collect::<Vec<_>>(),
        [JobState::Queued, JobState::Running]
    );
}

/// Asserts that unknown fields of every wire type inside a nested message (the job's times)
/// are skipped and the job decodes as written.
#[test]
fn unknown_fields_inside_a_nested_message_are_skipped() {
    let job = common::job();
    let times = with_unknown_fields(&job.times.expect("the sample has times").encode_to_vec());
    let mut bytes = Job {
        times: None,
        ..job.clone()
    }
    .encode_to_vec();
    encode_key(20, WireType::LengthDelimited, &mut bytes);
    encode_varint(
        u64::try_from(times.len()).expect("a small length"),
        &mut bytes,
    );
    bytes.extend_from_slice(&times);
    assert_eq!(Job::decode(bytes.as_slice()).expect("decodes"), job);
}

/// The longest tail of one stream a result or a log answer carries: 8 KiB.
const TAIL_CAP: usize = 8 * 1024;

/// A tail of `TAIL_CAP` bytes that holds NUL, bytes that are not UTF-8, and every other
/// byte value.
fn binary_tail() -> Vec<u8> {
    (0..TAIL_CAP)
        .map(|i| u8::try_from(i % 256).expect("below 256"))
        .collect()
}

/// Asserts that results at the boundaries of their fields come back from the wire as they
/// were written: an exit code of 0 and none (distinct), negative exit codes (a 32-bit
/// value encodes in ten bytes, as protobuf writes a negative int32), times of 0 and absent
/// (distinct) and at the largest value, usage at the limits of its types, and tails empty
/// and at the 8 KiB cap with NUL and bytes that are not UTF-8. The crate checks none of
/// these limits; a server does.
#[test]
fn results_at_their_boundaries_round_trip() {
    let exit = |code: Option<i32>| JobResult {
        exit_code: code,
        ..JobResult::default()
    };
    assert_eq!(exit(None).encode_to_vec(), Vec::<u8>::new());
    assert_eq!(exit(Some(0)).encode_to_vec(), [0x10, 0x00]);
    let mut minus_one = vec![0x10];
    minus_one.extend([0xff; 9]);
    minus_one.push(0x01);
    assert_eq!(exit(Some(-1)).encode_to_vec(), minus_one);

    let times = |placed: Option<u64>| JobTimes {
        queued_at_ms: u64::MAX,
        placed_at_ms: placed,
        started_at_ms: None,
        ended_at_ms: Some(u64::MAX),
    };
    assert_ne!(times(None).encode_to_vec(), times(Some(0)).encode_to_vec());

    let results = [
        exit(None),
        exit(Some(0)),
        exit(Some(-1)),
        exit(Some(i32::MIN)),
        exit(Some(i32::MAX)),
        JobResult {
            state: JobState::Succeeded.into(),
            exit_code: Some(0),
            times: Some(times(Some(0))),
            usage: Some(ResourceUsage {
                cpu_usec: u64::MAX,
                peak_memory_bytes: 0,
                disk_bytes: Some(0),
                gpus: u32::MAX,
            }),
            ..JobResult::default()
        },
        JobResult {
            stdout_tail: Vec::new(),
            stderr_tail: binary_tail(),
            truncated: true,
            times: Some(times(None)),
            ..JobResult::default()
        },
    ];
    for result in results {
        let bytes = result.encode_to_vec();
        assert_eq!(
            JobResult::decode(bytes.as_slice()).expect("decodes"),
            result
        );
    }

    let logs = GetLogsResponse {
        state: JobState::Running.into(),
        stdout_tail: binary_tail(),
        stderr_tail: Vec::new(),
        truncated: true,
    };
    let decoded = GetLogsResponse::decode(logs.encode_to_vec().as_slice()).expect("decodes");
    assert_eq!(decoded.stdout_tail.len(), TAIL_CAP);
    assert_eq!(decoded, logs);
}

/// Asserts that the details of a refusal's status are exactly the bytes of
/// `golden/refusal_status.hex`, which are written by hand from the field numbers of
/// `google.rpc.Status` and `google.protobuf.Any` and not by the crate, and that those bytes,
/// as another implementation would send them, decode through `refusal_of`. Catches a change
/// of the private wrapper's field numbers, which an encode and decode by the crate alone
/// would not see.
#[test]
fn refusal_status_writes_the_canonical_rich_error_bytes() {
    let golden = golden_bytes(include_str!("golden/refusal_status.hex"));
    let status = refusal_status(RefusalCode::JobIdConflict, "job-1 names another job");
    assert_eq!(status.details(), golden.as_slice());
    let sent_elsewhere = tonic::Status::with_details(
        tonic::Code::AlreadyExists,
        "job-1 names another job",
        golden.into(),
    );
    assert_eq!(
        refusal_of(&sent_elsewhere),
        Some(Refusal {
            code: RefusalCode::JobIdConflict.into(),
            detail: "job-1 names another job".to_owned(),
        })
    );
}

/// Asserts that the client field is additive. The field golden holds `Job.client` (32) and
/// `ListJobsRequest.client` (5) as optional strings beside every earlier row. A `Job` of the
/// previous schema (the golden bytes, written before the field) decodes with no client and
/// encodes back to the same bytes. A job with a client encodes it as one length-delimited
/// field of number 32 after the bytes of the previous schema, which a reader without the
/// field skips, reading every other field as written; a listing's filter is one field of
/// number 5. Catches a number reused from an earlier field (the golden would decode into the
/// client, or the earlier reader would read the client's bytes as another field) and a
/// renumbered or retyped client.
#[test]
fn the_client_field_is_additive_in_the_proto() {
    let rows = include_str!("golden/fields.txt");
    for row in [
        "Job.client = 32 : optional string",
        "ListJobsRequest.client = 5 : optional string",
        "Job.cancel_requested = 31 : bool",
        "ListJobsRequest.queued_before_ms = 4 : optional uint64",
    ] {
        assert!(rows.lines().any(|line| line == row), "{row}");
    }

    let previous = Job::decode(job_golden().as_slice()).expect("the golden decodes");
    assert_eq!(previous.client, None);
    assert_eq!(previous, common::job());
    assert_eq!(previous.encode_to_vec(), job_golden());

    let named = Job {
        client: Some("ci".to_owned()),
        ..common::job()
    };
    let mut bytes = job_golden();
    bytes.extend_from_slice(&[0x82, 0x02, 0x02, b'c', b'i']);
    assert_eq!(named.encode_to_vec(), bytes);
    assert_eq!(Job::decode(bytes.as_slice()).expect("decodes"), named);
    assert_eq!(
        LaterJob::decode(bytes.as_slice()).expect("an earlier reader decodes"),
        LaterJob::decode(job_golden().as_slice()).expect("the golden decodes")
    );

    let filter = ListJobsRequest {
        client: Some("ci".to_owned()),
        ..ListJobsRequest::default()
    };
    assert_eq!(filter.encode_to_vec(), [0x2a, 0x02, b'c', b'i']);
    assert_eq!(
        ListJobsRequest::decode(filter.encode_to_vec().as_slice()).expect("decodes"),
        filter
    );
}
