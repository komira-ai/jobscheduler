//! The isolation of a job (`Constraints.isolation`), held to the evolution rule of the
//! package: the field and its enum are additions, and a reader of the previous release meets
//! them as an unknown field.

use js_proto::FILE_DESCRIPTOR_SET;
use js_proto::v1::{Constraints, ExecMode, Isolation, JobSpec};
use prost::Message;
use prost_types::FileDescriptorSet;

/// The previous release's `Constraints`: operating system, architecture and execution mode.
/// A fixture standing for readers already deployed; it must not follow later changes.
#[derive(Clone, PartialEq, Message)]
struct PreviousConstraints {
    #[prost(string, tag = "1")]
    os: String,
    #[prost(string, tag = "2")]
    arch: String,
    #[prost(enumeration = "ExecMode", tag = "3")]
    exec: i32,
}

/// The previous release's `JobSpec` envelope, with the field the constraints travel in.
#[derive(Clone, PartialEq, Message)]
struct PreviousJobSpec {
    #[prost(message, optional, tag = "11")]
    constraints: Option<PreviousConstraints>,
}

/// The four golden rows that name the isolation, as the golden writes them.
const ADDED_ROWS: [&str; 4] = [
    "Constraints.isolation = 6 : Isolation",
    "Isolation.ISOLATION_UNSPECIFIED = 0",
    "Isolation.ISOLATION_CONTAINED = 1",
    "Isolation.ISOLATION_HOST = 2",
];

/// Asserts that the isolation is an addition on the wire and in the schema: the golden holds
/// its four rows and the descriptor holds the field as number 6 of `Constraints` and the
/// three values of `Isolation`; the previous release's encoded constraints decode with the
/// isolation unspecified; a new encoding with host isolation decodes in the previous reader
/// with every old field as sent and the new one skipped; and an encoding without the
/// isolation is byte-identical to the previous release's. Catches a renumbered or re-meant
/// field, a previous reader that cannot decode a host job, and a default that is written.
#[test]
fn isolation_is_additive_on_the_wire_and_in_the_proto() {
    let golden = include_str!("golden/fields.txt");
    for row in ADDED_ROWS {
        let held = golden.lines().filter(|line| *line == row).count();
        assert_eq!(held, 1, "{row}");
    }

    let set = FileDescriptorSet::decode(FILE_DESCRIPTOR_SET).expect("the descriptor set decodes");
    let constraints = set
        .file
        .iter()
        .flat_map(|file| &file.message_type)
        .find(|message| message.name() == "Constraints")
        .expect("Constraints is in the schema");
    let field = constraints
        .field
        .iter()
        .find(|field| field.name() == "isolation")
        .expect("Constraints.isolation is in the schema");
    assert_eq!(field.number(), 6);
    let isolation = set
        .file
        .iter()
        .flat_map(|file| &file.enum_type)
        .find(|one| one.name() == "Isolation")
        .expect("Isolation is in the schema");
    let values: Vec<(&str, i32)> = isolation
        .value
        .iter()
        .map(|value| (value.name(), value.number()))
        .collect();
    assert_eq!(
        values,
        [
            ("ISOLATION_UNSPECIFIED", 0),
            ("ISOLATION_CONTAINED", 1),
            ("ISOLATION_HOST", 2),
        ]
    );

    let previous = PreviousConstraints {
        os: "linux".to_owned(),
        arch: "x86_64".to_owned(),
        exec: ExecMode::Container as i32,
    };
    let old_bytes = previous.encode_to_vec();
    let read = Constraints::decode(old_bytes.as_slice()).expect("the new reader decodes");
    assert_eq!(read.isolation, Isolation::Unspecified as i32);
    assert_eq!(read.os, "linux");
    let same = Constraints {
        os: "linux".to_owned(),
        arch: "x86_64".to_owned(),
        exec: ExecMode::Container as i32,
        ..Constraints::default()
    };
    assert_eq!(same.encode_to_vec(), old_bytes);

    let host = JobSpec {
        constraints: Some(Constraints {
            isolation: Isolation::Host as i32,
            ..same
        }),
        ..JobSpec::default()
    };
    let bytes = host.encode_to_vec();
    let old_reader = PreviousJobSpec::decode(bytes.as_slice()).expect("the previous reader");
    assert_eq!(old_reader.constraints, Some(previous));
    let back = JobSpec::decode(bytes.as_slice()).expect("the new reader decodes");
    assert_eq!(back, host);
}
