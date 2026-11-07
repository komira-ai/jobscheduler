//! The checks of a submitted job (`validate.rs`), called directly: what a valid job becomes,
//! and the field each fault names, first fault first.

use std::collections::BTreeMap;
use std::time::Duration;

use js_jobs::{Constraints, DeclaredPort, Isolation, JobId, JobSpec, Needs, NetworkPolicy};
use js_proto::v1;
use js_types::JobNetwork as Reach;
use js_types::{
    AcceleratorNeed, ExecMode, ImageDigestRef, IsaLevel, Labels, Requirements, Resources,
};

use super::{Fault, ISA_LEVEL_KEY, Valid, job};
use crate::job_api::writes_rig::{CEILING, image, wire};

/// `spec` checked against the rig's ceiling.
fn checked(spec: &v1::JobSpec) -> Result<Valid, Fault> {
    job(Some(spec), CEILING)
}

/// The fault of `spec` changed by `change`.
fn fault(change: impl FnOnce(&mut v1::JobSpec)) -> Fault {
    let mut spec = wire("job-1");
    change(&mut spec);
    checked(&spec).expect_err("the job is refused")
}

/// The job `spec` changed by `change` becomes.
fn valid(change: impl FnOnce(&mut v1::JobSpec)) -> Valid {
    let mut spec = wire("job-1");
    change(&mut spec);
    checked(&spec).expect("the job is valid")
}

/// Asserts that every field of a job that sets all of them is recorded as given (the ISA
/// level and the accelerator among the capabilities, under the keys a lease request uses;
/// one command; the files' contents as bytes), with the priority beside the job, and that
/// admission is asked for the same resources, constraints, network and host ports. It
/// catches a field dropped on the way to the tables, which would make two different jobs
/// compare equal, and an admission that asks for less than the job.
#[test]
fn a_valid_job_becomes_the_recorded_job_and_what_admission_asks() {
    let accelerator = v1::Accelerator {
        vendor: Some("nvidia".to_owned()),
        model: Some("nvidia-a100".to_owned()),
        count: Some(2),
        memory_per_device_gib: Some(40),
        interconnect: Some("nvlink>=8".to_owned()),
        form_factor: Some("sxm".to_owned()),
    };
    let checked = valid(|spec| {
        spec.env = BTreeMap::from([("MODE".to_owned(), "fast".to_owned())]);
        spec.needs = Some(v1::Needs {
            cpu_millis: 4000,
            memory_bytes: 8 << 30,
            disk_bytes: Some(2 << 30),
            gpus: 2,
            gpu_class: "nvidia-a100".to_owned(),
            whole_host: false,
        });
        spec.constraints = Some(v1::Constraints {
            os: "linux".to_owned(),
            arch: "x86_64".to_owned(),
            exec: v1::ExecMode::Container.into(),
            isa_level: "x86-64-v3".to_owned(),
            accelerator: Some(accelerator),
            isolation: v1::Isolation::Unspecified.into(),
        });
        spec.network = v1::NetworkLevel::Open.into();
        spec.ports = vec![v1::PortForward {
            host_port: 8080,
            container_port: 80,
        }];
        spec.files = vec![v1::JobFile {
            name: "upload-url".to_owned(),
            contents: "secret".to_owned(),
        }];
        spec.priority = 7;
        spec.labels = BTreeMap::from([("team".to_owned(), "build".to_owned())]);
    });
    let resources = Resources::new(4000, 8 << 30, 2, Some("nvidia-a100".to_owned()))
        .unwrap()
        .with_disk_bytes(2 << 30);
    let capabilities: BTreeMap<String, String> = [
        (ISA_LEVEL_KEY, "x86-64-v3"),
        ("accelerator.vendor", "nvidia"),
        ("accelerator.model", "nvidia-a100"),
        ("accelerator.count", "2"),
        ("accelerator.memory-gib", "40"),
        ("accelerator.interconnect", "nvlink>=8"),
        ("accelerator.form-factor", "sxm"),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), value.to_owned()))
    .collect();
    let labels = BTreeMap::from([("team".to_owned(), "build".to_owned())]);
    let recorded = JobSpec {
        image: ImageDigestRef::parse(&image()).unwrap(),
        command: vec!["run".to_owned()],
        args: vec!["--fast".to_owned()],
        env: BTreeMap::from([("MODE".to_owned(), "fast".to_owned())]),
        needs: Needs::Shape(resources.clone()),
        constraints: Constraints {
            os: Some("linux".to_owned()),
            arch: Some("x86_64".to_owned()),
            exec: Some(ExecMode::Container),
            capabilities: capabilities.clone(),
            isolation: Isolation::Contained,
        },
        wall_limit_secs: 600,
        network: NetworkPolicy::Open,
        ports: vec![DeclaredPort {
            host: 8080,
            container: 80,
        }],
        files: js_jobs::JobFiles::Contents(BTreeMap::from([(
            "upload-url".to_owned(),
            b"secret".to_vec(),
        )])),
        labels: Labels::from_caller(&labels).unwrap(),
    };
    let accelerator = AcceleratorNeed::requested(&capabilities, &resources).unwrap();
    assert!(accelerator.is_some());
    let wanted = Requirements {
        resources,
        disk_bytes: Some(2 << 30),
        os: Some("linux".to_owned()),
        arch: Some("x86_64".to_owned()),
        isa_level: Some("x86-64-v3".parse::<IsaLevel>().unwrap()),
        exec: Some(ExecMode::Container),
        accelerator,
        network: Reach::Open,
        host_ports: vec![8080],
    };
    let expected = Valid {
        id: Some(JobId::parse("job-1").unwrap()),
        spec: recorded,
        priority: 7,
        wanted,
    };
    assert_eq!(checked, expected);
}

/// Asserts the defaults of what a job leaves out: no id (the store mints one), no command
/// (the image's), no needs (none), no constraints (any host), an unspecified network (none),
/// and that `aarch64` is recorded as `arm64`. It catches an empty id sent to the store, an
/// empty command recorded as a command of one empty word, and an architecture recorded in
/// two spellings, which would make one job two.
#[test]
fn what_a_job_leaves_out_is_recorded_as_its_default() {
    let checked = valid(|spec| {
        spec.job_id = String::new();
        spec.command = String::new();
        spec.args = Vec::new();
        spec.needs = None;
        spec.constraints = Some(v1::Constraints {
            arch: "aarch64".to_owned(),
            exec: v1::ExecMode::SandboxExec.into(),
            ..v1::Constraints::default()
        });
    });
    assert_eq!(checked.id, None);
    assert_eq!(checked.spec.command, Vec::<String>::new());
    assert_eq!(checked.spec.needs, Needs::Shape(Resources::default()));
    assert_eq!(checked.spec.constraints.arch.as_deref(), Some("arm64"));
    assert_eq!(checked.spec.constraints.exec, Some(ExecMode::SandboxExec));
    assert_eq!(checked.spec.constraints.capabilities, BTreeMap::new());
    assert_eq!(checked.spec.network, NetworkPolicy::None);
    assert_eq!(checked.wanted.disk_bytes, None);
    assert_eq!(checked.wanted.accelerator, None);
    for (level, policy, network) in [
        (v1::NetworkLevel::None, NetworkPolicy::None, Reach::None),
        (
            v1::NetworkLevel::AllowList,
            NetworkPolicy::AllowList,
            Reach::AllowList,
        ),
    ] {
        let checked = valid(|spec| spec.network = level.into());
        assert_eq!(
            (checked.spec.network, checked.wanted.network),
            (policy, network),
            "{level:?}"
        );
    }
}

/// Asserts that a whole-host job records the whole host and asks admission for a host of any
/// size, and that a whole-host job that names another need is refused naming `spec.needs`,
/// since the record could not hold it. It catches a whole-host job recorded as a share.
#[test]
fn a_whole_host_job_needs_nothing_else() {
    let checked = valid(|spec| {
        spec.needs = Some(v1::Needs {
            whole_host: true,
            ..v1::Needs::default()
        })
    });
    assert_eq!(checked.spec.needs, Needs::WholeHost);
    assert_eq!(checked.wanted.resources, Resources::default());
    let refused = fault(|spec| {
        spec.needs = Some(v1::Needs {
            whole_host: true,
            cpu_millis: 1,
            ..v1::Needs::default()
        })
    });
    assert_eq!(refused, Fault::Field("spec.needs"));
}

/// Asserts the GPU pair: a count takes its class from `gpu_class`, else from the
/// accelerator's model; a count with neither is refused naming `spec.needs.gpuClass`, a
/// class without a count `spec.needs.gpus`; and an accelerator model of another family than
/// the class is refused naming the accelerator. It catches a count booked with no class
/// (which no host could ever match) and a contradiction recorded as a job.
#[test]
fn a_gpu_count_needs_a_class() {
    let from_model = valid(|spec| {
        spec.needs = Some(v1::Needs {
            gpus: 2,
            ..v1::Needs::default()
        });
        spec.constraints = Some(v1::Constraints {
            accelerator: Some(v1::Accelerator {
                model: Some("nvidia-h100".to_owned()),
                ..v1::Accelerator::default()
            }),
            ..v1::Constraints::default()
        });
    });
    let booked = Resources::new(0, 0, 2, Some("nvidia-h100".to_owned())).unwrap();
    assert_eq!(from_model.spec.needs, Needs::Shape(booked));
    let gpus = |gpus: u32, class: &str| {
        fault(|spec| {
            spec.needs = Some(v1::Needs {
                gpus,
                gpu_class: class.to_owned(),
                ..v1::Needs::default()
            })
        })
    };
    assert_eq!(gpus(1, ""), Fault::Field("spec.needs.gpuClass"));
    assert_eq!(gpus(0, "nvidia-a100"), Fault::Field("spec.needs.gpus"));
    let contradicted = fault(|spec| {
        spec.needs = Some(v1::Needs {
            gpus: 1,
            gpu_class: "nvidia-a100".to_owned(),
            ..v1::Needs::default()
        });
        spec.constraints = Some(v1::Constraints {
            accelerator: Some(v1::Accelerator {
                model: Some("nvidia-h100".to_owned()),
                ..v1::Accelerator::default()
            }),
            ..v1::Constraints::default()
        });
    });
    assert_eq!(contradicted, Fault::Field("spec.constraints.accelerator"));
}

/// Asserts the wall limit's bounds: absent and 0 are `wall-time-not-positive`, the ceiling
/// plus one second `wall-time-above-ceiling`, and exactly the ceiling is accepted. It catches
/// an off-by-one at the ceiling and an absent limit read as unlimited.
#[test]
fn the_wall_limit_is_present_positive_and_at_most_the_ceiling() {
    assert_eq!(
        fault(|spec| spec.wall_limit_seconds = None),
        Fault::WallNotPositive
    );
    assert_eq!(
        fault(|spec| spec.wall_limit_seconds = Some(0)),
        Fault::WallNotPositive
    );
    let ceiling = CEILING.as_secs();
    assert_eq!(
        fault(|spec| spec.wall_limit_seconds = Some(ceiling + 1)),
        Fault::WallAboveCeiling
    );
    let at = valid(|spec| spec.wall_limit_seconds = Some(ceiling));
    assert_eq!(at.spec.wall_limit_secs, ceiling);
    let higher = job(Some(&wire("job-1")), Duration::from_secs(599));
    assert_eq!(higher, Err(Fault::WallAboveCeiling));
}

/// Asserts the field each fault names, one fault per case, and that the first field in the
/// message's order is the one named when two are wrong. It catches a check that lets a bad
/// value through to the tables and a refusal that names another field than the one at fault.
#[test]
fn each_fault_names_its_field() {
    assert_eq!(job(None, CEILING), Err(Fault::Field("spec")));
    let long = "a".repeat(64);
    for id in [long.as_str(), "a b", "a/b", "é"] {
        let refused = fault(|spec| spec.job_id = id.to_owned());
        assert_eq!(refused, Fault::Field("spec.jobId"), "{id:?}");
    }
    let longest = "a".repeat(63);
    let accepted = valid(|spec| spec.job_id = longest.clone());
    assert_eq!(accepted.id, Some(JobId::parse(&longest).unwrap()));
    let cases: Vec<(Fault, Change)> = vec![
        (
            Fault::Field("spec.image"),
            Box::new(|spec| spec.image = "registry.example/app:latest".to_owned()),
        ),
        (
            Fault::Field("spec.constraints.os"),
            Box::new(|spec| spec.constraints = Some(constraints(|c| c.os = "windows".to_owned()))),
        ),
        (
            Fault::Field("spec.constraints.exec"),
            Box::new(|spec| spec.constraints = Some(constraints(|c| c.exec = 9))),
        ),
        (
            Fault::Field("spec.constraints.isolation"),
            Box::new(|spec| spec.constraints = Some(constraints(|c| c.isolation = 9))),
        ),
        (
            Fault::Field("spec.constraints.isaLevel"),
            Box::new(|spec| {
                spec.constraints = Some(constraints(|c| c.isa_level = "x86-64-v9".to_owned()))
            }),
        ),
        (
            Fault::Field("spec.constraints.accelerator"),
            Box::new(|spec| {
                let accelerator = v1::Accelerator {
                    vendor: Some("NVIDIA!".to_owned()),
                    ..v1::Accelerator::default()
                };
                spec.constraints = Some(constraints(|c| c.accelerator = Some(accelerator)))
            }),
        ),
        (
            Fault::Field("spec.network"),
            Box::new(|spec| spec.network = 9),
        ),
        (
            Fault::Port(0, "hostPort"),
            Box::new(|spec| spec.ports = vec![forward(0, 80)]),
        ),
        (
            Fault::Port(0, "hostPort"),
            Box::new(|spec| spec.ports = vec![forward(65_536, 80)]),
        ),
        (
            Fault::Port(1, "hostPort"),
            Box::new(|spec| spec.ports = vec![forward(8080, 80), forward(8080, 81)]),
        ),
        (
            Fault::Port(0, "containerPort"),
            Box::new(|spec| spec.ports = vec![forward(8080, 0)]),
        ),
        (
            Fault::Field("spec.files"),
            Box::new(|spec| {
                spec.files = vec![v1::JobFile {
                    name: "../escape".to_owned(),
                    contents: String::new(),
                }]
            }),
        ),
        (
            Fault::Field("spec.labels"),
            Box::new(|spec| {
                spec.labels = BTreeMap::from([("jobscheduler.dev/run".to_owned(), "x".to_owned())])
            }),
        ),
    ];
    for (expected, change) in cases {
        assert_eq!(fault(change), expected);
    }
    let both = fault(|spec| {
        spec.job_id = "a b".to_owned();
        spec.image = "registry.example/app:latest".to_owned();
    });
    assert_eq!(both, Fault::Field("spec.jobId"), "the first field is named");
}

/// A change a fault case makes to the job.
type Change = Box<dyn FnOnce(&mut v1::JobSpec)>;

/// The constraints changed by `change` from none.
fn constraints(change: impl FnOnce(&mut v1::Constraints)) -> v1::Constraints {
    let mut constraints = v1::Constraints::default();
    change(&mut constraints);
    constraints
}

/// A port forward from `host` to `container`.
fn forward(host: u32, container: u32) -> v1::PortForward {
    v1::PortForward {
        host_port: host,
        container_port: container,
    }
}

/// Asserts that admission is asked for the host ports sorted, each once, whatever order the
/// job declares them in, while the job records them as declared. It catches requirements
/// that break their documented order, which a host's ceiling check reports from.
#[test]
fn admission_is_asked_for_the_host_ports_in_order() {
    let checked = valid(|spec| spec.ports = vec![forward(9000, 90), forward(8000, 80)]);
    assert_eq!(checked.wanted.host_ports, [8000, 9000]);
    let declared: Vec<u16> = checked.spec.ports.iter().map(|port| port.host).collect();
    assert_eq!(declared, [9000, 8000]);
}

/// Asserts that an unspecified or contained isolation is recorded as contained and a host
/// isolation as host, and that the isolation is part of the recorded job. Catches a mapping
/// that drops the field, which would run a host job in a container, or reads unspecified as
/// host.
#[test]
fn isolation_is_recorded_as_given_and_unspecified_is_contained() {
    for (given, recorded) in [
        (v1::Isolation::Unspecified, Isolation::Contained),
        (v1::Isolation::Contained, Isolation::Contained),
        (v1::Isolation::Host, Isolation::Host),
    ] {
        let checked = valid(|spec| {
            spec.constraints = Some(constraints(|c| c.isolation = given.into()));
        });
        assert_eq!(checked.spec.constraints.isolation, recorded);
    }
}
