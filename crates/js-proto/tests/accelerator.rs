//! The accelerator fields of the job (`Constraints.accelerator`, `Constraints.isa_level`)
//! and their conversion into a booking, held to the evolution rule of the package.

use js_caps::{AcceleratorRequest, RequestNeedError};
use js_proto::request_need;
use js_proto::v1::{Accelerator, Constraints, ExecMode, Job, JobSpec, Needs};
use prost::Message;

/// The bytes of a golden `.hex` file: hex pairs separated by white space, each line's text
/// from `#` on a comment.
fn golden_bytes(text: &str) -> Vec<u8> {
    text.lines()
        .map(|line| line.split('#').next().unwrap_or_default())
        .flat_map(str::split_whitespace)
        .map(|pair| u8::from_str_radix(pair, 16).expect("a hex pair"))
        .collect()
}

/// The shipped rows of `golden/fields.txt` that name the new fields, as the golden writes
/// them.
const ADDED_ROWS: [&str; 8] = [
    "Constraints.isa_level = 4 : string",
    "Constraints.accelerator = 5 : Accelerator",
    "Accelerator.vendor = 1 : optional string",
    "Accelerator.model = 2 : optional string",
    "Accelerator.count = 3 : optional uint32",
    "Accelerator.memory_per_device_gib = 4 : optional uint32",
    "Accelerator.interconnect = 5 : optional string",
    "Accelerator.form_factor = 6 : optional string",
];

fn needs(gpus: u32, class: &str) -> Needs {
    Needs {
        gpus,
        gpu_class: class.to_owned(),
        ..Needs::default()
    }
}

fn constraints(accelerator: Accelerator) -> Constraints {
    Constraints {
        accelerator: Some(accelerator),
        ..Constraints::default()
    }
}

/// Asserts that the proto grew only by addition: the field golden holds the new rows beside
/// every old one; the previous schema's encoded job decodes with no accelerator and an empty
/// `isa_level`, and re-encodes to the same bytes; a job with only the old `gpus` and
/// `gpu_class` converts to the need `js_caps::request_need` gives the pair; and a count-only
/// accelerator is refused with `CountWithoutModel`, while an accelerator with a model books
/// its count. Catches a renumbered or re-meant field, a new field that a previous release's
/// bytes do not decode without, and a conversion that reads the pair differently from the
/// lease body.
#[test]
fn the_proto_accelerator_fields_are_additive() {
    let golden = include_str!("golden/fields.txt");
    for row in ADDED_ROWS {
        assert_eq!(
            golden.lines().filter(|line| *line == row).count(),
            1,
            "{row}"
        );
    }
    for row in [
        "Constraints.os = 1 : string",
        "Constraints.arch = 2 : string",
        "Constraints.exec = 3 : ExecMode",
        "Needs.gpus = 4 : uint32",
        "Needs.gpu_class = 5 : string",
    ] {
        assert_eq!(
            golden.lines().filter(|line| *line == row).count(),
            1,
            "{row}"
        );
    }

    let bytes = golden_bytes(include_str!("golden/job.hex"));
    let job = Job::decode(bytes.as_slice()).expect("the previous schema's job decodes");
    let spec = job.spec.expect("the job holds its spec");
    let old_constraints = spec.constraints.expect("the job holds its constraints");
    assert_eq!(old_constraints.accelerator, None);
    assert_eq!(old_constraints.isa_level, "");
    assert_eq!(old_constraints.arch, "x86_64");
    let old_needs = spec.needs.expect("the job holds its needs");
    assert_eq!(
        request_need(&old_needs, Some(&old_constraints)),
        js_caps::request_need(u64::from(old_needs.gpus), Some(&old_needs.gpu_class), None)
    );
    assert_eq!(
        Job::decode(bytes.as_slice())
            .expect("the golden decodes")
            .encode_to_vec(),
        bytes
    );

    // Only the old pair, with no constraints at all, and with no class.
    let pair =
        js_caps::request_need(1, Some("nvidia-a100-sxm4-80gb"), None).expect("the pair converts");
    assert_eq!(
        request_need(&needs(1, "nvidia-a100-sxm4-80gb"), None),
        Ok(pair)
    );
    assert_eq!(
        request_need(&needs(0, ""), Some(&Constraints::default())),
        js_caps::request_need(0, None, None)
    );

    // An accelerator with a model books its count of that model.
    let accelerator = Accelerator {
        vendor: Some("nvidia".to_owned()),
        model: Some("nvidia-a100".to_owned()),
        count: Some(2),
        memory_per_device_gib: Some(40),
        interconnect: Some("nvlink>=300".to_owned()),
        form_factor: Some("sxm".to_owned()),
    };
    let request = AcceleratorRequest {
        vendor: Some("nvidia".to_owned()),
        model: Some("nvidia-a100".to_owned()),
        count: Some(2),
        memory_per_device_gib: Some(40),
        interconnect: Some("nvlink>=300".to_owned()),
        form_factor: Some("sxm".to_owned()),
    };
    let converted = request_need(&needs(0, ""), Some(&constraints(accelerator)));
    assert_eq!(converted, js_caps::request_need(0, None, Some(&request)));
    let converted = converted.expect("the accelerator converts");
    assert_eq!(converted.gpus, 2);
    assert!(!converted.capabilities.is_empty());

    // A count with no model is refused.
    let count_only = Accelerator {
        count: Some(2),
        ..Accelerator::default()
    };
    assert_eq!(
        request_need(&needs(0, ""), Some(&constraints(count_only))),
        Err(RequestNeedError::CountWithoutModel)
    );
}

/// Asserts that a spec carrying an accelerator and an `isa_level` survives an encode and a
/// decode with every field kept. Catches a new field that is dropped on the wire.
#[test]
fn the_new_constraint_fields_round_trip() {
    let spec = JobSpec {
        constraints: Some(Constraints {
            isa_level: "x86-64-v3".to_owned(),
            accelerator: Some(Accelerator {
                model: Some("nvidia-h100".to_owned()),
                count: Some(8),
                ..Accelerator::default()
            }),
            ..Constraints::default()
        }),
        ..JobSpec::default()
    };
    let decoded = JobSpec::decode(spec.encode_to_vec().as_slice()).expect("the spec decodes");
    assert_eq!(decoded, spec);
}

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

/// The previous release's `JobSpec` envelope, with the fields the constraints travel with.
#[derive(Clone, PartialEq, Message)]
struct PreviousJobSpec {
    #[prost(string, tag = "1")]
    job_id: String,
    #[prost(message, optional, tag = "10")]
    needs: Option<Needs>,
    #[prost(message, optional, tag = "11")]
    constraints: Option<PreviousConstraints>,
}

fn new_spec() -> JobSpec {
    JobSpec {
        job_id: "job-1".to_owned(),
        needs: Some(needs(1, "nvidia-a100-sxm4-80gb")),
        constraints: Some(Constraints {
            os: "linux".to_owned(),
            arch: "x86_64".to_owned(),
            exec: ExecMode::Container as i32,
            isa_level: "x86-64-v3".to_owned(),
            accelerator: Some(Accelerator {
                vendor: Some("nvidia".to_owned()),
                model: Some("nvidia-a100".to_owned()),
                count: Some(2),
                memory_per_device_gib: Some(40),
                interconnect: Some("nvlink>=300".to_owned()),
                form_factor: Some("sxm".to_owned()),
            }),
        }),
        ..JobSpec::default()
    }
}

/// Asserts that the constraints of a new spec, with `isa_level` and a populated
/// accelerator, decode in the previous release's `Constraints` with `os`, `arch` and `exec`
/// exactly as sent, that the previous `JobSpec` keeps the `Needs` pair `gpus` and
/// `gpu_class`, and that decoding succeeds with the new fields skipped. Catches a new field
/// that a reader already deployed cannot decode, or one that disturbs a legacy field.
#[test]
fn a_previous_reader_decodes_new_constraints() {
    let bytes = new_spec().encode_to_vec();
    let previous = PreviousJobSpec::decode(bytes.as_slice()).expect("the previous reader decodes");
    assert_eq!(previous.job_id, "job-1");
    let constraints = previous.constraints.expect("the constraints are kept");
    assert_eq!(constraints.os, "linux");
    assert_eq!(constraints.arch, "x86_64");
    assert_eq!(constraints.exec, ExecMode::Container as i32);
    let needs = previous.needs.expect("the needs are kept");
    assert_eq!(needs.gpus, 1);
    assert_eq!(needs.gpu_class, "nvidia-a100-sxm4-80gb");

    let alone = new_spec()
        .constraints
        .expect("the constraints are set")
        .encode_to_vec();
    let alone = PreviousConstraints::decode(alone.as_slice()).expect("the constraints decode");
    assert_eq!(
        alone,
        PreviousConstraints {
            os: "linux".to_owned(),
            arch: "x86_64".to_owned(),
            exec: ExecMode::Container as i32,
        }
    );
}
