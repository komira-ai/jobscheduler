//! The wire format of the messages: fully populated messages against hand-reviewable
//! golden bytes, decoding of bytes from a newer schema, and the defaults a submit gets for
//! what it leaves out.

mod common;

use js_proto::FILE_DESCRIPTOR_SET;
use js_proto::v1::{Job, JobSpec, NetworkLevel, SubmitJobRequest};
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

/// `bytes` with two fields appended that no message of this version has: a varint and a
/// length-delimited one, numbered far above any number in use.
fn with_unknown_fields(bytes: &[u8]) -> Vec<u8> {
    let mut out = bytes.to_vec();
    encode_key(100_000, WireType::Varint, &mut out);
    encode_varint(42, &mut out);
    encode_key(100_001, WireType::LengthDelimited, &mut out);
    encode_varint(3, &mut out);
    out.extend_from_slice(b"new");
    out
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
