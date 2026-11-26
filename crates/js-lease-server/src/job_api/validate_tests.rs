//! The checks of a submitted job (`validate.rs`), called directly: what a valid job becomes,
//! and the field each fault names, first fault first.

use std::collections::BTreeMap;
use std::time::Duration;

use js_agent_wire::{HostPort, JobMount, Layer, MountMode, MountSource, PortProtocol, Propagation};
use js_caps::{FLAG_SET, LAYERED_ROOTFS};
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
            os_version: "26".to_owned(),
            accelerator: Some(accelerator),
            isolation: v1::Isolation::Unspecified.into(),
            xcode: String::new(),
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
        ("os_version", "26"),
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
        image: ImageDigestRef::parse(&image()).ok(),
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
        layers: Vec::new(),
        mounts: Vec::new(),
        host_ports: Vec::new(),
        history_key: Default::default(),
        retry: None,
    };
    let accelerator = AcceleratorNeed::requested(&capabilities, &resources).unwrap();
    assert!(accelerator.is_some());
    let wanted = Requirements {
        resources,
        disk_bytes: Some(2 << 30),
        os: Some("linux".to_owned()),
        arch: Some("x86_64".to_owned()),
        isa_level: Some("x86-64-v3".parse::<IsaLevel>().unwrap()),
        os_version: Some(26),
        exec: Some(ExecMode::Container),
        accelerator,
        network: Reach::Open,
        host_ports: vec![8080],
        layered_rootfs: false,
        mount_providers: Vec::new(),
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

/// Asserts that a least `os_version` of one to four digits without a leading zero is recorded
/// under the `os_version` capability as the same text and asked of admission as that number
/// (`1`, `26` and `9999` are the bounds and a typical value), and that an empty one records
/// no capability and asks for none. Catches a version recorded in another spelling, which
/// would make one job two, and one dropped on the way to admission, which would place a
/// macOS 26 job on a macOS 15 host.
#[test]
fn a_least_os_version_is_recorded_as_its_number_and_empty_asks_for_none() {
    for (given, asked) in [("1", 1), ("26", 26), ("9999", 9999)] {
        let checked = valid(|spec| {
            spec.constraints = Some(constraints(|c| c.os_version = given.to_owned()));
        });
        assert_eq!(
            checked.spec.constraints.capabilities.get("os_version"),
            Some(&given.to_owned())
        );
        assert_eq!(checked.wanted.os_version, Some(asked));
    }
    let none = valid(|spec| spec.constraints = Some(constraints(|_| {})));
    assert_eq!(none.spec.constraints.capabilities.get("os_version"), None);
    assert_eq!(none.wanted.os_version, None);
}

/// Asserts that an Xcode build (`16C5032a`, and one of exactly 32 bytes) is recorded under the
/// `xcode` capability as the same text, and that an empty one records no capability. Catches a
/// build dropped on the way to the tables, which would place an Xcode job on any Mac.
#[test]
fn an_xcode_build_is_recorded_as_its_text_and_empty_records_nothing() {
    let longest = "A".repeat(32);
    for given in ["16C5032a", "16E140", "15F31d", "1.2_3-4", longest.as_str()] {
        let checked = valid(|spec| {
            spec.constraints = Some(constraints(|c| c.xcode = given.to_owned()));
        });
        assert_eq!(
            checked.spec.constraints.capabilities.get("xcode"),
            Some(&given.to_owned())
        );
    }
    let none = valid(|spec| spec.constraints = Some(constraints(|_| {})));
    assert_eq!(none.spec.constraints.capabilities.get("xcode"), None);
}

/// Asserts that a build with a comma, a leading dot, 33 bytes or a space is refused naming
/// `spec.constraints.xcode`. Catches a value that would split into two builds in the host's
/// comma-joined set, or one the agent's start check would refuse after the job was placed.
#[test]
fn a_malformed_xcode_build_is_refused_naming_its_field() {
    let long = "A".repeat(33);
    for given in [
        "16C5032a,16E140",
        ".16C",
        long.as_str(),
        "16C 5032a",
        "-16C",
    ] {
        let refused = fault(|spec| {
            spec.constraints = Some(constraints(|c| c.xcode = given.to_owned()));
        });
        assert_eq!(refused, Fault::Field("spec.constraints.xcode"), "{given:?}");
    }
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
            Fault::Field("spec.constraints.osVersion"),
            Box::new(|spec| {
                spec.constraints = Some(constraints(|c| c.os_version = "26.1".to_owned()))
            }),
        ),
        (
            Fault::Field("spec.constraints.osVersion"),
            Box::new(|spec| {
                spec.constraints = Some(constraints(|c| c.os_version = "0".to_owned()))
            }),
        ),
        (
            Fault::Field("spec.constraints.osVersion"),
            Box::new(|spec| {
                spec.constraints = Some(constraints(|c| c.os_version = "026".to_owned()))
            }),
        ),
        (
            Fault::Field("spec.constraints.osVersion"),
            Box::new(|spec| {
                spec.constraints = Some(constraints(|c| c.os_version = "12345".to_owned()))
            }),
        ),
        (
            Fault::Field("spec.constraints.osVersion"),
            Box::new(|spec| {
                spec.constraints = Some(constraints(|c| c.os_version = "+26".to_owned()))
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

/// A job's layers: each a host directory `/layer/<n>`.
fn host_layers(count: u32) -> Vec<v1::Layer> {
    (0..count)
        .map(|at| v1::Layer {
            source: Some(v1::layer::Source::HostPath(format!("/layer/{at}"))),
        })
        .collect()
}

/// One mount of `/src` at `/data`.
fn one_mount() -> Vec<v1::Mount> {
    vec![v1::Mount {
        source: Some(v1::mount::Source::HostPath("/src".to_owned())),
        target: "/data".to_owned(),
        ..v1::Mount::default()
    }]
}

/// One host port from `host`, to `job` when given.
fn one_host_port(host: u32, job: Option<u32>) -> Vec<v1::HostPort> {
    vec![v1::HostPort {
        host_port: host,
        container_port: job,
        protocol: v1::PortProtocol::Unspecified.into(),
    }]
}

/// The capabilities a job asks of its host when it uses the fields of a layered root.
fn layered_capabilities() -> BTreeMap<String, String> {
    BTreeMap::from([(LAYERED_ROOTFS.to_owned(), FLAG_SET.to_owned())])
}

/// Asserts that an empty image is refused naming `spec.image` when the job has no layers and
/// accepted, recorded as no image, when it has some (mounts and host ports alone do not
/// stand in). It catches a job with no root at all reaching the tables, and a layered job
/// recorded with an image it does not have.
#[test]
fn an_empty_image_needs_layers() {
    for change in [
        (|spec: &mut v1::JobSpec| spec.image = String::new()) as fn(&mut v1::JobSpec),
        |spec| {
            spec.image = String::new();
            spec.mounts = one_mount();
        },
        |spec| {
            spec.image = String::new();
            spec.host_ports = one_host_port(80, None);
        },
    ] {
        assert_eq!(fault(change), Fault::Field("spec.image"));
    }
    let layered = valid(|spec| {
        spec.image = String::new();
        spec.command = "/bin/run".to_owned();
        spec.layers = host_layers(1);
    });
    assert_eq!(layered.spec.image, None);
    assert_eq!(layered.spec.layers.len(), 1);
}

/// Asserts that a job with an empty image takes its command from the job: a relative or an
/// empty command is refused naming `spec.command`, an absolute one accepted, and that a
/// job with an image may keep a relative command. It catches a layered job that would start
/// with no program to run, there being no image configuration to name one.
#[test]
fn an_imageless_job_needs_an_absolute_command() {
    for command in ["run", "", "./run", "/bin/../run", "/bin/"] {
        let refused = fault(|spec| {
            spec.image = String::new();
            spec.layers = host_layers(1);
            spec.command = command.to_owned();
        });
        assert_eq!(refused, Fault::Field("spec.command"), "{command:?}");
    }
    let accepted = valid(|spec| {
        spec.image = String::new();
        spec.layers = host_layers(1);
        spec.command = "/bin/run".to_owned();
    });
    assert_eq!(accepted.spec.command, ["/bin/run"]);
    let with_image = valid(|spec| {
        spec.layers = host_layers(1);
        spec.command = "run".to_owned();
    });
    assert_eq!(with_image.spec.command, ["run"]);
}

/// Asserts that host isolation is refused naming `spec.constraints.isolation` with layers,
/// with mounts and with host ports, each alone, and accepted without them. It catches a
/// host-isolated job, which runs on the host's own root, given a root of its own.
#[test]
fn host_isolation_is_refused_with_any_rootfs_field() {
    let host = Some(constraints(|c| c.isolation = v1::Isolation::Host.into()));
    let changes: Vec<Change> = vec![
        Box::new(|spec| spec.layers = host_layers(1)),
        Box::new(|spec| spec.mounts = one_mount()),
        Box::new(|spec| spec.host_ports = one_host_port(80, None)),
    ];
    for change in changes {
        let refused = fault(|spec| {
            change(spec);
            spec.constraints = host.clone();
        });
        assert_eq!(refused, Fault::Field("spec.constraints.isolation"));
    }
    let plain = valid(|spec| spec.constraints = host.clone());
    assert_eq!(plain.spec.constraints.isolation, Isolation::Host);
}

/// Asserts that a whole-host job is refused naming `spec.needs` with layers, with mounts and
/// with host ports, each alone, and accepted without them. It catches a job the whole-host
/// pass never places (it does not hold a job to the hosts that report `layered-rootfs`) taken
/// and left queued until it expires.
#[test]
fn a_whole_host_job_is_refused_with_any_rootfs_field() {
    let whole = Some(v1::Needs {
        whole_host: true,
        ..v1::Needs::default()
    });
    let changes: Vec<Change> = vec![
        Box::new(|spec| spec.layers = host_layers(1)),
        Box::new(|spec| spec.mounts = one_mount()),
        Box::new(|spec| spec.host_ports = one_host_port(80, None)),
    ];
    for change in changes {
        let refused = fault(|spec| {
            change(spec);
            spec.needs = whole.clone();
        });
        assert_eq!(refused, Fault::Field("spec.needs"));
    }
    let plain = valid(|spec| spec.needs = whole.clone());
    assert_eq!(plain.spec.needs, Needs::WholeHost);
}

/// Asserts that sandbox-exec is refused naming `spec.constraints.exec` with layers and with
/// a host port whose job port differs from its host port (given), and accepted with a host
/// port at its own number (given or absent) and with mounts alone. It catches a job asking
/// for a root or a network the sandbox cannot give.
#[test]
fn sandbox_exec_is_refused_for_layers_and_a_remapped_port() {
    let sandbox = Some(constraints(|c| c.exec = v1::ExecMode::SandboxExec.into()));
    let refused: Vec<Change> = vec![
        Box::new(|spec| spec.layers = host_layers(1)),
        Box::new(|spec| spec.host_ports = one_host_port(80, Some(8080))),
        Box::new(|spec| {
            spec.host_ports = [one_host_port(80, None), one_host_port(81, Some(82))].concat()
        }),
    ];
    for change in refused {
        let fault = fault(|spec| {
            change(spec);
            spec.constraints = sandbox.clone();
        });
        assert_eq!(fault, Fault::Field("spec.constraints.exec"));
    }
    let accepted: Vec<Change> = vec![
        Box::new(|spec| spec.host_ports = one_host_port(80, Some(80))),
        Box::new(|spec| spec.host_ports = one_host_port(80, None)),
        Box::new(|spec| spec.mounts = one_mount()),
    ];
    for change in accepted {
        let checked = valid(|spec| {
            change(spec);
            spec.constraints = sandbox.clone();
        });
        assert_eq!(checked.spec.constraints.exec, Some(ExecMode::SandboxExec));
        assert_eq!(checked.wanted.exec, Some(ExecMode::SandboxExec));
    }
}

/// Asserts that an unspecified exec is recorded as container for layers and for a remapped
/// host port, in the record and in what admission asks, is left unset for mounts alone and
/// for an unremapped host port, and that an explicit container is kept. It catches a
/// layered job that is placed on a host that cannot stack a root.
#[test]
fn exec_is_container_when_the_job_needs_one() {
    let container: Vec<Change> = vec![
        Box::new(|spec| spec.layers = host_layers(2)),
        Box::new(|spec| spec.host_ports = one_host_port(80, Some(8080))),
        Box::new(|spec| {
            spec.mounts = one_mount();
            spec.constraints = Some(constraints(|c| c.exec = v1::ExecMode::Container.into()));
        }),
    ];
    for change in container {
        let checked = valid(change);
        assert_eq!(checked.spec.constraints.exec, Some(ExecMode::Container));
        assert_eq!(checked.wanted.exec, Some(ExecMode::Container));
    }
    let unset: Vec<Change> = vec![
        Box::new(|spec| spec.mounts = one_mount()),
        Box::new(|spec| spec.host_ports = one_host_port(80, None)),
        Box::new(|spec| spec.host_ports = one_host_port(80, Some(80))),
    ];
    for change in unset {
        let checked = valid(change);
        assert_eq!(checked.spec.constraints.exec, None);
        assert_eq!(checked.wanted.exec, None);
    }
}

/// Asserts that each of layers, mounts and host ports alone records the `layered-rootfs`
/// capability and asks admission for it, and that a job with none records no capability and
/// does not ask. It catches a layered job placed on a host that does not report the
/// capability, and a plain job refused by one that does not.
#[test]
fn each_rootfs_field_alone_asks_for_the_layered_rootfs_capability() {
    let each: Vec<Change> = vec![
        Box::new(|spec| spec.layers = host_layers(1)),
        Box::new(|spec| spec.mounts = one_mount()),
        Box::new(|spec| spec.host_ports = one_host_port(80, None)),
    ];
    for change in each {
        let checked = valid(change);
        assert_eq!(
            checked.spec.constraints.capabilities,
            layered_capabilities()
        );
        assert!(checked.wanted.layered_rootfs);
    }
    let none = valid(|_| {});
    assert_eq!(none.spec.constraints.capabilities, BTreeMap::new());
    assert!(!none.wanted.layered_rootfs);
}

/// Asserts that a job with layers, mounts and host ports becomes the record in explicit
/// form (defaults spelled out, an empty subPath none, the job port filled in) with a
/// container exec, the capability and no image, and that admission is asked for the host
/// port's host number. It catches a field dropped or left in its default spelling on the way
/// to the record.
#[test]
fn a_job_with_layers_mounts_and_host_ports_becomes_the_explicit_record() {
    let checked = valid(|spec| {
        spec.image = String::new();
        spec.command = "/bin/run".to_owned();
        spec.layers = vec![
            v1::Layer {
                source: Some(v1::layer::Source::HostPath("/base".to_owned())),
            },
            v1::Layer {
                source: Some(v1::layer::Source::Image(image())),
            },
        ];
        spec.mounts = vec![
            v1::Mount {
                source: Some(v1::mount::Source::HostPath("/data".to_owned())),
                target: "/in".to_owned(),
                ..v1::Mount::default()
            },
            v1::Mount {
                source: Some(v1::mount::Source::HostPath("/out".to_owned())),
                target: "/mnt/out".to_owned(),
                mode: v1::MountMode::ReadWrite.into(),
                sub_path: "results".to_owned(),
                no_exec: true,
                propagation: v1::MountPropagation::Private.into(),
            },
        ];
        spec.host_ports = vec![v1::HostPort {
            host_port: 5432,
            container_port: None,
            protocol: v1::PortProtocol::Unspecified.into(),
        }];
    });
    assert_eq!(checked.spec.image, None);
    assert_eq!(
        checked.spec.layers,
        [
            Layer::HostPath {
                path: "/base".to_owned()
            },
            Layer::Image { image: image() },
        ]
    );
    assert_eq!(
        checked.spec.mounts,
        [
            JobMount {
                source: MountSource::HostPath {
                    path: "/data".to_owned()
                },
                target: "/in".to_owned(),
                mode: MountMode::ReadOnly,
                sub_path: None,
                no_exec: false,
                propagation: Propagation::Private,
            },
            JobMount {
                source: MountSource::HostPath {
                    path: "/out".to_owned()
                },
                target: "/mnt/out".to_owned(),
                mode: MountMode::ReadWrite,
                sub_path: Some("results".to_owned()),
                no_exec: true,
                propagation: Propagation::Private,
            },
        ]
    );
    assert_eq!(
        checked.spec.host_ports,
        [HostPort {
            host_port: 5432,
            container_port: Some(5432),
            protocol: PortProtocol::Tcp,
        }]
    );
    assert_eq!(checked.spec.constraints.exec, Some(ExecMode::Container));
    assert_eq!(
        checked.spec.constraints.capabilities,
        layered_capabilities()
    );
    assert!(checked.wanted.layered_rootfs);
}

/// A layer from the provider `name`, asked for `reference`.
fn provider_layer(name: &str, reference: &str) -> v1::Layer {
    v1::Layer {
        source: Some(v1::layer::Source::Provider(v1::ProviderSource {
            provider: name.to_owned(),
            r#ref: reference.to_owned(),
        })),
    }
}

/// A mount at `target` from the provider `name`, asked for `reference`.
fn provider_mount(name: &str, reference: &str, target: &str) -> v1::Mount {
    v1::Mount {
        source: Some(v1::mount::Source::Provider(v1::ProviderSource {
            provider: name.to_owned(),
            r#ref: reference.to_owned(),
        })),
        target: target.to_owned(),
        ..v1::Mount::default()
    }
}

/// Asserts that a job whose layers and mounts name providers records the capability
/// `mount-provider.<name>=true` once for each distinct name (two layers of one provider, and a
/// mount of it, are one key), beside `layered-rootfs`, lists the names sorted in what admission
/// asks, and keeps the sources in the record as given; and that a job that names none, and a
/// job of host paths, records no provider key. Catches a provider named twice recorded twice, a
/// provider of a mount missed, a name that reaches placement but not admission, and a plain
/// job that asks for a provider.
#[test]
fn a_job_naming_providers_records_a_capability_for_each_distinct_one() {
    let checked = valid(|spec| {
        spec.layers = vec![
            provider_layer("tierstore", "trees/a"),
            provider_layer("tierstore", "trees/b"),
        ];
        spec.mounts = vec![
            provider_mount("cache", "inputs", "/cache"),
            provider_mount("tierstore", "trees/c", "/more"),
        ];
    });
    let mut expected = layered_capabilities();
    for name in ["cache", "tierstore"] {
        expected.insert(format!("mount-provider.{name}"), FLAG_SET.to_owned());
    }
    assert_eq!(checked.spec.constraints.capabilities, expected);
    assert_eq!(checked.wanted.mount_providers, ["cache", "tierstore"]);
    assert!(checked.wanted.layered_rootfs);
    assert_eq!(
        checked.spec.layers,
        [
            Layer::Provider {
                provider: "tierstore".to_owned(),
                reference: "trees/a".to_owned(),
            },
            Layer::Provider {
                provider: "tierstore".to_owned(),
                reference: "trees/b".to_owned(),
            },
        ]
    );
    assert_eq!(
        checked.spec.mounts[0].source,
        MountSource::Provider {
            provider: "cache".to_owned(),
            reference: "inputs".to_owned(),
        }
    );
    assert_eq!(checked.spec.constraints.exec, Some(ExecMode::Container));
    let hosted = valid(|spec| spec.mounts = one_mount());
    assert_eq!(hosted.spec.constraints.capabilities, layered_capabilities());
    assert!(hosted.wanted.mount_providers.is_empty());
    assert!(valid(|_| {}).wanted.mount_providers.is_empty());
}

/// Asserts that a provider mount alone, with no layer, records its provider's capability and
/// leaves `exec` unspecified (a mount needs no container of its own), and that a provider
/// source is refused with a whole-host need (`spec.needs`) and with host isolation
/// (`spec.constraints.isolation`), as any layer or mount is. Catches a provider job that the
/// whole-host pass or a host-isolated run would take without its tree.
#[test]
fn a_provider_source_is_held_to_the_rules_of_any_rootfs_field() {
    let alone = valid(|spec| spec.mounts = vec![provider_mount("tierstore", "k", "/in")]);
    assert_eq!(alone.wanted.mount_providers, ["tierstore"]);
    assert_eq!(alone.spec.constraints.exec, None);
    let changes: Vec<Change> = vec![
        Box::new(|spec| spec.layers = vec![provider_layer("tierstore", "k")]),
        Box::new(|spec| spec.mounts = vec![provider_mount("tierstore", "k", "/in")]),
    ];
    let whole = Some(v1::Needs {
        whole_host: true,
        ..v1::Needs::default()
    });
    let host = Some(constraints(|c| c.isolation = v1::Isolation::Host.into()));
    for change in changes {
        let mut spec = wire("job-1");
        change(&mut spec);
        let wholly = v1::JobSpec {
            needs: whole.clone(),
            ..spec.clone()
        };
        assert_eq!(checked(&wholly).unwrap_err(), Fault::Field("spec.needs"));
        let hosted = v1::JobSpec {
            constraints: host.clone(),
            ..spec
        };
        let refused = checked(&hosted).unwrap_err();
        assert_eq!(refused, Fault::Field("spec.constraints.isolation"));
    }
}
