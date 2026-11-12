//! The rootfs fields of a job (`JobSpec.layers`, `mounts` and `host_ports`, the input failure
//! of a result and the start failure code that names it), held to the evolution rule of the
//! package: every one is an addition, and a reader of the previous release meets them as an
//! unknown field or as an enum number it does not name.

use js_proto::FILE_DESCRIPTOR_SET;
use js_proto::v1::layer::Source as LayerSource;
use js_proto::v1::mount::Source as MountSource;
use js_proto::v1::{
    HostPort, Job, JobResult, JobSpec, JobState, Layer, Mount, MountMode, MountPropagation,
    PortProtocol, ResultReason, StartFailureCode,
};
use prost::Message;
use prost_types::{DescriptorProto, FileDescriptorSet};

/// The previous release's `PortForward`. A fixture standing for readers already deployed; it
/// must not follow later changes.
#[derive(Clone, PartialEq, Message)]
struct PreviousPortForward {
    #[prost(uint32, tag = "1")]
    host_port: u32,
    #[prost(uint32, tag = "2")]
    container_port: u32,
}

/// The previous release's `JobSpec`: no host ports, layers or mounts.
#[derive(Clone, PartialEq, Message)]
struct PreviousJobSpec {
    #[prost(string, tag = "1")]
    job_id: String,
    #[prost(string, tag = "2")]
    image: String,
    #[prost(string, tag = "3")]
    command: String,
    #[prost(string, repeated, tag = "4")]
    args: Vec<String>,
    #[prost(uint64, optional, tag = "20")]
    wall_limit_seconds: Option<u64>,
    #[prost(message, repeated, tag = "31")]
    ports: Vec<PreviousPortForward>,
    #[prost(int32, tag = "50")]
    priority: i32,
}

/// The previous release's `JobResult`: no input failure, and the reason as a number.
#[derive(Clone, PartialEq, Message)]
struct PreviousJobResult {
    #[prost(int32, tag = "1")]
    state: i32,
    #[prost(int32, optional, tag = "2")]
    exit_code: Option<i32>,
    #[prost(int32, tag = "3")]
    reason: i32,
    #[prost(bytes = "vec", tag = "4")]
    stdout_tail: Vec<u8>,
    #[prost(bool, tag = "6")]
    truncated: bool,
}

/// The previous release's `Job`, with the start failure as a number.
#[derive(Clone, PartialEq, Message)]
struct PreviousJob {
    #[prost(string, tag = "1")]
    id: String,
    #[prost(int32, tag = "3")]
    state: i32,
    #[prost(int32, tag = "4")]
    start_failure: i32,
    #[prost(string, tag = "5")]
    start_failure_detail: String,
}

/// A `Mount` as a later release could send it: a source this release does not know (field 3)
/// and the enums as raw numbers.
#[derive(Clone, PartialEq, Message)]
struct LaterMount {
    #[prost(string, tag = "3")]
    tmpfs: String,
    #[prost(string, tag = "10")]
    target: String,
    #[prost(int32, tag = "11")]
    mode: i32,
    #[prost(int32, tag = "14")]
    propagation: i32,
}

/// A `Layer` as a later release could send it: a source this release does not know (field 3).
#[derive(Clone, PartialEq, Message)]
struct LaterLayer {
    #[prost(string, tag = "3")]
    registry: String,
}

/// A `HostPort` as a later release could send it, with the protocol as a raw number.
#[derive(Clone, PartialEq, Message)]
struct LaterHostPort {
    #[prost(uint32, tag = "1")]
    host_port: u32,
    #[prost(int32, tag = "3")]
    protocol: i32,
}

/// The golden rows that name the rootfs additions, as the golden writes them.
const ADDED_ROWS: [&str; 24] = [
    "Layer.host_path = 1 : string (oneof source)",
    "Layer.image = 2 : string (oneof source)",
    "MountMode.MOUNT_MODE_UNSPECIFIED = 0",
    "MountMode.MOUNT_MODE_READ_ONLY = 1",
    "MountMode.MOUNT_MODE_READ_WRITE = 2",
    "MountMode.MOUNT_MODE_COPY_ON_WRITE = 3",
    "MountPropagation.MOUNT_PROPAGATION_UNSPECIFIED = 0",
    "MountPropagation.MOUNT_PROPAGATION_PRIVATE = 1",
    "Mount.host_path = 1 : string (oneof source)",
    "Mount.target = 10 : string",
    "Mount.mode = 11 : MountMode",
    "Mount.sub_path = 12 : string",
    "Mount.no_exec = 13 : bool",
    "Mount.propagation = 14 : MountPropagation",
    "PortProtocol.PORT_PROTOCOL_UNSPECIFIED = 0",
    "PortProtocol.PORT_PROTOCOL_TCP = 1",
    "HostPort.host_port = 1 : uint32",
    "HostPort.container_port = 2 : optional uint32",
    "HostPort.protocol = 3 : PortProtocol",
    "JobSpec.host_ports = 32 : repeated HostPort",
    "JobSpec.layers = 60 : repeated Layer",
    "JobSpec.mounts = 61 : repeated Mount",
    "StartFailureCode.START_FAILURE_CODE_INPUT_UNAVAILABLE = 5",
    "ResultReason.RESULT_REASON_INPUT_FAILED = 5",
];

/// The descriptor set of the package, decoded.
fn schema() -> FileDescriptorSet {
    FileDescriptorSet::decode(FILE_DESCRIPTOR_SET).expect("the descriptor set decodes")
}

/// The message called `name` in the schema.
fn message<'a>(set: &'a FileDescriptorSet, name: &str) -> &'a DescriptorProto {
    set.file
        .iter()
        .flat_map(|file| &file.message_type)
        .find(|message| message.name() == name)
        .unwrap_or_else(|| panic!("{name} is in the schema"))
}

/// The fields of the message `name` as (field name, number, oneof name) rows.
fn fields(set: &FileDescriptorSet, name: &str) -> Vec<(String, i32, Option<String>)> {
    let one = message(set, name);
    one.field
        .iter()
        .map(|field| {
            let oneof = field
                .oneof_index
                .filter(|_| !field.proto3_optional())
                .map(|at| {
                    one.oneof_decl[usize::try_from(at).expect("a non-negative index")]
                        .name()
                        .to_owned()
                });
            (field.name().to_owned(), field.number(), oneof)
        })
        .collect()
}

/// The values of the enum `name` as (value name, number) rows.
fn values(set: &FileDescriptorSet, name: &str) -> Vec<(String, i32)> {
    set.file
        .iter()
        .flat_map(|file| &file.enum_type)
        .find(|one| one.name() == name)
        .unwrap_or_else(|| panic!("{name} is in the schema"))
        .value
        .iter()
        .map(|value| (value.name().to_owned(), value.number()))
        .collect()
}

/// A spec that uses all three new lists, with its old fields set.
fn new_spec() -> JobSpec {
    JobSpec {
        job_id: "job-1".to_owned(),
        image: "".to_owned(),
        command: "/bin/run".to_owned(),
        args: vec!["--fast".to_owned()],
        wall_limit_seconds: Some(60),
        ports: vec![js_proto::v1::PortForward {
            host_port: 8080,
            container_port: 80,
        }],
        priority: 3,
        layers: vec![
            Layer {
                source: Some(LayerSource::HostPath("/srv/base".to_owned())),
            },
            Layer {
                source: Some(LayerSource::Image("sha256:abc".to_owned())),
            },
        ],
        mounts: vec![Mount {
            source: Some(MountSource::HostPath("/srv/data".to_owned())),
            target: "/data".to_owned(),
            mode: MountMode::CopyOnWrite as i32,
            sub_path: "inner".to_owned(),
            no_exec: true,
            propagation: MountPropagation::Private as i32,
        }],
        host_ports: vec![
            HostPort {
                host_port: 9000,
                container_port: Some(9001),
                protocol: PortProtocol::Tcp as i32,
            },
            HostPort {
                host_port: 9100,
                container_port: None,
                protocol: PortProtocol::Unspecified as i32,
            },
        ],
        ..JobSpec::default()
    }
}

/// The previous release's view of the old fields of `new_spec`.
fn previous_spec() -> PreviousJobSpec {
    PreviousJobSpec {
        job_id: "job-1".to_owned(),
        image: "".to_owned(),
        command: "/bin/run".to_owned(),
        args: vec!["--fast".to_owned()],
        wall_limit_seconds: Some(60),
        ports: vec![PreviousPortForward {
            host_port: 8080,
            container_port: 80,
        }],
        priority: 3,
    }
}

/// Asserts that every row of the rootfs additions is in the golden exactly once. Catches an
/// addition that the golden does not pin, and a row written twice.
#[test]
fn the_golden_holds_each_rootfs_row_once() {
    let golden = include_str!("golden/fields.txt");
    for row in ADDED_ROWS {
        let held = golden.lines().filter(|line| *line == row).count();
        assert_eq!(held, 1, "{row}");
    }
}

/// Asserts the numbers the schema gives the new fields and enum values, read by name from the
/// descriptor set: the three lists of the spec, the input failure of the result, the fields of
/// `Mount`, `Layer` and `HostPort` with their oneof, and the enum values. Catches a renumbered
/// or renamed field, a source that left its oneof, and an enum value that moved.
#[test]
fn the_schema_numbers_the_rootfs_fields_and_values() {
    let set = schema();
    let spec = fields(&set, "JobSpec");
    for (name, number) in [("host_ports", 32), ("layers", 60), ("mounts", 61)] {
        assert!(
            spec.contains(&(name.to_owned(), number, None)),
            "JobSpec.{name} = {number}"
        );
    }
    assert!(fields(&set, "JobResult").contains(&("input_failure".to_owned(), 12, None)));
    let source = || Some("source".to_owned());
    assert_eq!(
        fields(&set, "Layer"),
        [
            ("host_path".to_owned(), 1, source()),
            ("image".to_owned(), 2, source()),
        ]
    );
    assert_eq!(
        fields(&set, "Mount"),
        [
            ("host_path".to_owned(), 1, source()),
            ("target".to_owned(), 10, None),
            ("mode".to_owned(), 11, None),
            ("sub_path".to_owned(), 12, None),
            ("no_exec".to_owned(), 13, None),
            ("propagation".to_owned(), 14, None),
        ]
    );
    assert_eq!(
        fields(&set, "HostPort"),
        [
            ("host_port".to_owned(), 1, None),
            ("container_port".to_owned(), 2, None),
            ("protocol".to_owned(), 3, None),
        ]
    );
    let named = |rows: &[(&str, i32)]| -> Vec<(String, i32)> {
        rows.iter()
            .map(|(name, number)| ((*name).to_owned(), *number))
            .collect()
    };
    assert_eq!(
        values(&set, "MountMode"),
        named(&[
            ("MOUNT_MODE_UNSPECIFIED", 0),
            ("MOUNT_MODE_READ_ONLY", 1),
            ("MOUNT_MODE_READ_WRITE", 2),
            ("MOUNT_MODE_COPY_ON_WRITE", 3),
        ])
    );
    assert_eq!(
        values(&set, "MountPropagation"),
        named(&[
            ("MOUNT_PROPAGATION_UNSPECIFIED", 0),
            ("MOUNT_PROPAGATION_PRIVATE", 1),
        ])
    );
    assert_eq!(
        values(&set, "PortProtocol"),
        named(&[("PORT_PROTOCOL_UNSPECIFIED", 0), ("PORT_PROTOCOL_TCP", 1)])
    );
    assert!(
        values(&set, "StartFailureCode")
            .contains(&("START_FAILURE_CODE_INPUT_UNAVAILABLE".to_owned(), 5))
    );
    assert!(values(&set, "ResultReason").contains(&("RESULT_REASON_INPUT_FAILED".to_owned(), 5)));
}

/// Asserts that a new spec with layers, mounts and host ports decodes in the previous reader
/// with every old field as sent and the new lists skipped, and that what the previous reader
/// writes back reads in the new type with the three lists empty (a previous server drops
/// them). Catches a new field that a previous reader cannot decode, and one that disturbs an
/// old field.
#[test]
fn a_previous_reader_keeps_the_old_fields_of_a_spec_with_rootfs_lists() {
    let spec = new_spec();
    let bytes = spec.encode_to_vec();
    let old = PreviousJobSpec::decode(bytes.as_slice()).expect("the previous reader decodes");
    assert_eq!(old, previous_spec());

    let rewritten = old.encode_to_vec();
    let back = JobSpec::decode(rewritten.as_slice()).expect("the new reader decodes");
    assert!(back.layers.is_empty());
    assert!(back.mounts.is_empty());
    assert!(back.host_ports.is_empty());
    assert_eq!(
        back,
        JobSpec {
            layers: vec![],
            mounts: vec![],
            host_ports: vec![],
            ..spec.clone()
        }
    );

    let same = JobSpec::decode(bytes.as_slice()).expect("the new reader decodes");
    assert_eq!(same, spec);
}

/// Asserts that bytes of the previous release's spec decode in the new type with the three
/// lists empty and the old fields as sent, and re-encode byte-identical. Catches a default
/// that is written, and a changed encoding of an old field.
#[test]
fn old_spec_bytes_decode_with_empty_lists_and_encode_unchanged() {
    let old_bytes = previous_spec().encode_to_vec();
    let read = JobSpec::decode(old_bytes.as_slice()).expect("the new reader decodes");
    assert!(read.layers.is_empty());
    assert!(read.mounts.is_empty());
    assert!(read.host_ports.is_empty());
    assert_eq!(read.job_id, "job-1");
    assert_eq!(read.command, "/bin/run");
    assert_eq!(read.priority, 3);
    assert_eq!(read.encode_to_vec(), old_bytes);
}

/// Asserts that a previous result reader meets a new result with an input failure and the new
/// reason with every old field as sent, reading the reason as the raw number 5, and that the
/// new reader reads the reason as `InputFailed`; and that a previous job reader reads the new
/// start failure as the raw number 5 with its detail. Catches a new field or enum value that
/// breaks a previous reader.
#[test]
fn a_previous_reader_meets_the_input_failure_and_the_new_codes() {
    let result = JobResult {
        state: JobState::Failed as i32,
        exit_code: Some(7),
        reason: ResultReason::InputFailed as i32,
        stdout_tail: b"out".to_vec(),
        truncated: true,
        input_failure: Some("layer 1 vanished".to_owned()),
        ..JobResult::default()
    };
    let bytes = result.encode_to_vec();
    let old = PreviousJobResult::decode(bytes.as_slice()).expect("the previous reader decodes");
    assert_eq!(
        old,
        PreviousJobResult {
            state: JobState::Failed as i32,
            exit_code: Some(7),
            reason: 5,
            stdout_tail: b"out".to_vec(),
            truncated: true,
        }
    );
    let back = JobResult::decode(bytes.as_slice()).expect("the new reader decodes");
    assert_eq!(back.reason(), ResultReason::InputFailed);
    assert_eq!(back, result);

    let old_bytes = old.encode_to_vec();
    let read = JobResult::decode(old_bytes.as_slice()).expect("the new reader decodes");
    assert_eq!(read.input_failure, None);
    assert_eq!(read.encode_to_vec(), old_bytes);

    let job = Job {
        id: "job-1".to_owned(),
        state: JobState::FailedToStart as i32,
        start_failure: StartFailureCode::InputUnavailable as i32,
        start_failure_detail: "input-unavailable".to_owned(),
        ..Job::default()
    };
    let job_bytes = job.encode_to_vec();
    let old_job = PreviousJob::decode(job_bytes.as_slice()).expect("the previous reader decodes");
    assert_eq!(
        old_job,
        PreviousJob {
            id: "job-1".to_owned(),
            state: JobState::FailedToStart as i32,
            start_failure: 5,
            start_failure_detail: "input-unavailable".to_owned(),
        }
    );
}

/// Asserts that numbers this release does not name are kept as raw numbers: a mount mode, a
/// propagation and a port protocol from a later release survive a decode and an encode, and
/// the typed accessors read them as their default. Catches a reader that refuses a later
/// release's enum value or rewrites it.
#[test]
fn unknown_enum_numbers_are_kept_as_raw_numbers() {
    let later = LaterMount {
        tmpfs: String::new(),
        target: "/data".to_owned(),
        mode: 99,
        propagation: 77,
    };
    let bytes = later.encode_to_vec();
    let mount = Mount::decode(bytes.as_slice()).expect("the new reader decodes");
    assert_eq!(mount.mode, 99);
    assert_eq!(mount.propagation, 77);
    assert_eq!(mount.mode(), MountMode::Unspecified);
    assert_eq!(mount.propagation(), MountPropagation::Unspecified);
    assert_eq!(mount.encode_to_vec(), bytes);

    let later = LaterHostPort {
        host_port: 9000,
        protocol: 55,
    };
    let bytes = later.encode_to_vec();
    let port = HostPort::decode(bytes.as_slice()).expect("the new reader decodes");
    assert_eq!(port.protocol, 55);
    assert_eq!(port.protocol(), PortProtocol::Unspecified);
    assert_eq!(port.encode_to_vec(), bytes);
}

/// Asserts that a source field number this release does not know, as a later release's tmpfs
/// mount or registry layer would send, reads as no source with the other fields intact, and
/// that a spec holding them still decodes. Catches a reader that refuses an unknown source or
/// reads it as another one.
#[test]
fn an_unknown_source_reads_as_none_with_the_other_fields_intact() {
    let later = LaterMount {
        tmpfs: "64m".to_owned(),
        target: "/scratch".to_owned(),
        mode: MountMode::ReadWrite as i32,
        propagation: 0,
    };
    let bytes = later.encode_to_vec();
    let mount = Mount::decode(bytes.as_slice()).expect("the new reader decodes");
    assert_eq!(mount.source, None);
    assert_eq!(mount.target, "/scratch");
    assert_eq!(mount.mode(), MountMode::ReadWrite);

    let layer_bytes = LaterLayer {
        registry: "example".to_owned(),
    }
    .encode_to_vec();
    let layer = Layer::decode(layer_bytes.as_slice()).expect("the new reader decodes");
    assert_eq!(layer.source, None);

    let mut spec_bytes = JobSpec {
        job_id: "job-2".to_owned(),
        ..JobSpec::default()
    }
    .encode_to_vec();
    for (tag, body) in [(61u32, &bytes), (60u32, &layer_bytes)] {
        let mut key = Vec::new();
        prost::encoding::encode_key(tag, prost::encoding::WireType::LengthDelimited, &mut key);
        spec_bytes.extend(key);
        prost::encoding::encode_varint(body.len() as u64, &mut spec_bytes);
        spec_bytes.extend(body.iter());
    }
    let spec = JobSpec::decode(spec_bytes.as_slice()).expect("the new reader decodes");
    assert_eq!(spec.job_id, "job-2");
    assert_eq!(spec.mounts.len(), 1);
    assert_eq!(spec.mounts[0].source, None);
    assert_eq!(spec.layers, [Layer { source: None }]);
}
