//! The start a placement is sent, and every reason one is not.

use std::collections::BTreeMap;

use js_agent_wire::{
    BookedResources, EGRESS_CAPABILITY, EGRESS_PORTS_CAPABILITY, HostPort, Isolation, JobMount,
    Layer, MountMode, MountSource, NetworkCeiling, PortProtocol, Propagation, RunFile,
};
use js_jobs::{AllocationRecord, Booked, DeclaredPort, JobFiles, JobId, JobRecord, NetworkPolicy};
use js_types::{HostId, JobNetwork as Level, PortMap, Resources};

use super::{Unsendable, start_of};
use crate::agent_api::job_rig::{BOOKED, image, placed, spec, tables};
use crate::metrics::NoMetrics;
use crate::per_job::BeatContext;

/// A placed job's records, with the spec's network, ports, environment and files set.
fn records() -> (JobRecord, AllocationRecord) {
    let (_store, jobs) = tables();
    let mut job_spec = spec();
    job_spec.network = NetworkPolicy::AllowList;
    job_spec.ports = vec![DeclaredPort {
        host: 8080,
        container: 80,
    }];
    job_spec.env = BTreeMap::from([("MODE".to_owned(), "test".to_owned())]);
    job_spec.files = JobFiles::Contents(BTreeMap::from([(
        "upload".to_owned(),
        b"https://store/put".to_vec(),
    )]));
    placed(&jobs, "job-a", "host-a", job_spec, BOOKED)
}

/// The start of `job` for `allocation` to `host-a`, whose agent reports `ceiling` and no
/// `host-jobs`.
fn build(
    job: &JobRecord,
    allocation: &AllocationRecord,
    ceiling: &NetworkCeiling,
) -> Result<js_agent_wire::JobStartSpec, Unsendable> {
    build_on(job, allocation, ceiling, None)
}

/// [`build`], for a host the registry books as `shape`.
fn build_on(
    job: &JobRecord,
    allocation: &AllocationRecord,
    ceiling: &NetworkCeiling,
    shape: Option<&Resources>,
) -> Result<js_agent_wire::JobStartSpec, Unsendable> {
    build_for(job, allocation, (ceiling, false), shape)
}

/// The start of `job` for `allocation` to `host-a`, whose agent reports `ceiling` and, when
/// `host_jobs`, that it runs host jobs, and which the registry books as `shape`.
fn build_for(
    job: &JobRecord,
    allocation: &AllocationRecord,
    (ceiling, host_jobs): (&NetworkCeiling, bool),
    shape: Option<&Resources>,
) -> Result<js_agent_wire::JobStartSpec, Unsendable> {
    let host = HostId::new("host-a").unwrap();
    start_of(
        job,
        allocation,
        &BeatContext {
            host: &host,
            ceiling,
            host_jobs,
            shape,
            logs: None,
            metrics: &NoMetrics,
            layered_rootfs: false,
        },
    )
}

/// A host whose agent gives allow-list network and forwards host port 8080.
fn allow_list() -> NetworkCeiling {
    let capabilities = BTreeMap::from([
        (EGRESS_CAPABILITY.to_owned(), "allow-list".to_owned()),
        (EGRESS_PORTS_CAPABILITY.to_owned(), "8080".to_owned()),
    ]);
    NetworkCeiling::from_capabilities(&capabilities).unwrap()
}

/// The start carries the job as stored (the command then its arguments, the environment,
/// the files as text, the network level and the ports) with what its allocation booked (no
/// disk field when it books none) and the wall limit in milliseconds. It catches a start that
/// drops a field or carries the job's request instead of the booking.
#[test]
fn a_start_carries_the_job_and_what_its_allocation_booked() {
    let (job, mut allocation) = records();
    let start = build(&job, &allocation, &allow_list()).unwrap();
    assert_eq!(start.job_id, "job-a");
    assert_eq!(start.allocation_id, allocation.allocation_id.to_string());
    assert_eq!(start.image, image());
    assert_eq!(start.command, ["run", "--fast"]);
    assert_eq!(start.env.get("MODE").map(String::as_str), Some("test"));
    assert_eq!(
        start.files,
        [RunFile {
            name: "upload".to_owned(),
            contents: "https://store/put".to_owned(),
        }]
    );
    assert_eq!(start.network, Level::AllowList);
    assert_eq!(
        start.ports,
        [PortMap {
            host: 8080,
            container: 80,
        }]
    );
    assert_eq!(start.wall_ms, 600_000);
    assert_eq!(start.booked.disk_bytes, Some(2 << 30));

    allocation.booked = Booked::Shape {
        cpu_millis: 500,
        memory_bytes: 1 << 20,
        disk_bytes: 0,
        accelerators: 0,
    };
    let mut open = job.clone();
    open.spec.network = NetworkPolicy::Open;
    open.spec.ports.clear();
    let ceiling = NetworkCeiling {
        level: Level::Open,
        ports: Vec::new(),
    };
    let start = build(&open, &allocation, &ceiling).unwrap();
    assert_eq!(start.network, Level::Open);
    assert_eq!(
        start.booked,
        BookedResources {
            cpu_millis: 500,
            memory_bytes: 1 << 20,
            disk_bytes: None,
            gpu_indices: Vec::new(),
        }
    );
}

/// Each start the wire cannot carry is refused with its reason: a whole-host booking when the
/// host's shape is not known, GPUs booked by count, a network level or a port above the
/// host's ceiling, a file that is not text, a wall limit that overflows, and a job id the
/// wire's grammar refuses. It catches a start sent with something the host would refuse or
/// misread.
#[test]
fn a_start_the_wire_cannot_carry_says_why() {
    let (job, allocation) = records();
    let ceiling = allow_list();
    let mut whole = allocation.clone();
    whole.booked = Booked::WholeHost;
    assert_eq!(build(&job, &whole, &ceiling), Err(Unsendable::NoHostShape));
    let mut gpus = allocation.clone();
    gpus.booked = Booked::Shape {
        cpu_millis: 1000,
        memory_bytes: 1 << 30,
        disk_bytes: 0,
        accelerators: 2,
    };
    assert_eq!(build(&job, &gpus, &ceiling), Err(Unsendable::GpuIndices(2)));
    assert_eq!(
        build(&job, &allocation, &NetworkCeiling::default()),
        Err(Unsendable::AboveCeiling)
    );
    let mut unforwarded = job.clone();
    unforwarded.spec.ports[0].host = 9090;
    assert_eq!(
        build(&unforwarded, &allocation, &ceiling),
        Err(Unsendable::AboveCeiling)
    );
    let mut binary = job.clone();
    let mut files = binary.spec.files.contents().cloned().unwrap();
    files.insert("blob".to_owned(), vec![0xff, 0xfe]);
    binary.spec.files = JobFiles::Contents(files);
    assert_eq!(
        build(&binary, &allocation, &ceiling),
        Err(Unsendable::FileNotText("blob".to_owned()))
    );
    // An ended job's files are digests: there are no contents to send, and none is made up.
    let mut digested = job.clone();
    digested.spec.files = JobFiles::Digests(BTreeMap::from([(
        "upload".to_owned(),
        js_jobs::FileDigest::of(b"https://store/put"),
    )]));
    assert_eq!(
        build(&digested, &allocation, &ceiling),
        Err(Unsendable::FilesDigested)
    );
    assert_eq!(
        Unsendable::FilesDigested.to_string(),
        "the job's files hold digests, not contents"
    );
    let mut endless = job.clone();
    endless.spec.wall_limit_secs = u64::MAX;
    assert_eq!(
        build(&endless, &allocation, &ceiling),
        Err(Unsendable::WallLimit)
    );
    let mut dashed = job;
    dashed.id = JobId::parse("-job").unwrap();
    assert_eq!(
        build(&dashed, &allocation, &ceiling),
        Err(Unsendable::Invalid {
            code: "invalid-job",
            field: "job_id",
        })
    );
}

/// A job on a contained host carries `Isolation::Contained` (the wire leaves it out), and a
/// job that runs on the machine itself carries `Isolation::Host` to a host whose agent
/// reported `host-jobs`; the same start to a host that did not report it is refused with its
/// reason, whatever else the host gives. It catches an isolation dropped from the start, so
/// a host job runs contained, and a host job sent to an agent that cannot run it.
#[test]
fn a_host_job_start_carries_its_isolation_and_goes_only_to_a_host_that_reports_host_jobs() {
    let (job, allocation) = records();
    let ceiling = allow_list();
    let contained = build_for(&job, &allocation, (&ceiling, true), None).unwrap();
    assert_eq!(contained.isolation, Isolation::Contained);
    let mut on_the_machine = job.clone();
    on_the_machine.spec.constraints.isolation = js_jobs::Isolation::Host;
    let host = build_for(&on_the_machine, &allocation, (&ceiling, true), None).unwrap();
    assert_eq!(host.isolation, Isolation::Host);
    assert_eq!(
        build_for(&on_the_machine, &allocation, (&ceiling, false), None),
        Err(Unsendable::HostJobsNotReported)
    );
    assert_eq!(
        Unsendable::HostJobsNotReported.to_string(),
        "the job runs on the machine itself and the host does not report host-jobs"
    );
}

/// A start of an allocation that books the whole host books the host's shape as the registry
/// holds it (CPU, memory and, when it books any, disk), with no device indices; its wall limit
/// is the allocation's when it records one and the job's otherwise. A host with GPUs cannot be
/// started on, since a start names each device by its index. It catches a whole-host start
/// that books nothing or a share, one that drops the disk, one that names invented devices,
/// and a wall limit cut for a backfill that the start does not carry.
#[test]
fn a_whole_host_start_books_the_hosts_shape_and_the_allocations_wall_limit() {
    let (job, mut allocation) = records();
    allocation.booked = Booked::WholeHost;
    let ceiling = allow_list();
    let host = Resources::new(8000, 16 << 30, 0, None).unwrap();
    let start = build_on(&job, &allocation, &ceiling, Some(&host)).unwrap();
    assert_eq!(
        start.booked,
        BookedResources {
            cpu_millis: 8000,
            memory_bytes: 16 << 30,
            disk_bytes: None,
            gpu_indices: Vec::new(),
        }
    );
    assert_eq!(start.wall_ms, 600_000);

    let with_disk = host.clone().with_disk_bytes(5 << 30);
    allocation.wall_limit_secs = Some(90);
    let start = build_on(&job, &allocation, &ceiling, Some(&with_disk)).unwrap();
    assert_eq!(start.booked.disk_bytes, Some(5 << 30));
    assert_eq!(start.wall_ms, 90_000);

    let gpus = Resources::new(8000, 16 << 30, 2, Some("a100".to_owned())).unwrap();
    assert_eq!(
        build_on(&job, &allocation, &ceiling, Some(&gpus)),
        Err(Unsendable::GpuIndices(2))
    );
}

/// The start of `job` for `allocation` to `host-a`, whose agent reports `ceiling` and, when
/// `layered_rootfs`, the `layered-rootfs` capability.
fn build_layered(
    job: &JobRecord,
    allocation: &AllocationRecord,
    ceiling: &NetworkCeiling,
    layered_rootfs: bool,
) -> Result<js_agent_wire::JobStartSpec, Unsendable> {
    let host = HostId::new("host-a").unwrap();
    start_of(
        job,
        allocation,
        &BeatContext {
            host: &host,
            ceiling,
            host_jobs: false,
            shape: None,
            logs: None,
            metrics: &NoMetrics,
            layered_rootfs,
        },
    )
}

/// A host-path layer.
fn host_layer(path: &str) -> Layer {
    Layer::HostPath {
        path: path.to_owned(),
    }
}

/// A mount of the host directory `source` at `target`, with every option at its default.
fn mount_of(source: &str, target: &str) -> JobMount {
    JobMount {
        source: MountSource::HostPath {
            path: source.to_owned(),
        },
        target: target.to_owned(),
        mode: MountMode::ReadOnly,
        sub_path: None,
        no_exec: false,
        propagation: Propagation::Private,
    }
}

/// A host port the job reaches at the same port.
fn host_port(port: u16) -> HostPort {
    HostPort {
        host_port: port,
        container_port: None,
        protocol: PortProtocol::Tcp,
    }
}

/// A job's layers (a host path and an image), mounts (one with every option set) and host
/// ports (one remapped) are carried into the start exactly as the record holds them, in
/// order, beside the image, and are built only for a host that reports `layered-rootfs`. It
/// catches a start that drops a list, reorders it, or rewrites an option.
#[test]
fn a_start_carries_the_layers_mounts_and_host_ports_as_the_record_holds_them() {
    let (mut job, allocation) = records();
    let layers = vec![
        host_layer("/opt/tools"),
        Layer::Image {
            image: format!("team/base@sha256:{}", "b".repeat(64)),
        },
    ];
    let mounts = vec![
        mount_of("/data/in", "/in"),
        JobMount {
            mode: MountMode::CopyOnWrite,
            sub_path: Some("sub/dir".to_owned()),
            no_exec: true,
            ..mount_of("/data/work", "/cache")
        },
    ];
    let host_ports = vec![
        host_port(5432),
        HostPort {
            container_port: Some(6000),
            ..host_port(7000)
        },
    ];
    job.spec.layers.clone_from(&layers);
    job.spec.mounts.clone_from(&mounts);
    job.spec.host_ports.clone_from(&host_ports);
    let start = build_layered(&job, &allocation, &allow_list(), true).unwrap();
    assert_eq!(start.layers, layers);
    assert_eq!(start.mounts, mounts);
    assert_eq!(start.host_ports, host_ports);
    assert_eq!(start.image, image());
}

/// A named change that gives a job one of the three rootfs fields.
type Field = (&'static str, fn(&mut JobRecord));

/// A job with layers, mounts or host ports, each alone, is not sent to a host that does not
/// report `layered-rootfs`, with the text the log and the start timeout show; sent to one that
/// does, each builds. It catches a job started on an agent that would ignore the fields and
/// run it on the image alone, and a trigger that needs two of the three.
#[test]
fn a_job_with_any_rootfs_field_is_not_sent_to_a_host_without_layered_rootfs() {
    let (job, allocation) = records();
    let ceiling = allow_list();
    let fields: [Field; 3] = [
        ("layers", |job| {
            job.spec.layers = vec![host_layer("/opt/tools")]
        }),
        ("mounts", |job| {
            job.spec.mounts = vec![mount_of("/data/in", "/in")];
        }),
        ("host ports", |job| {
            job.spec.host_ports = vec![host_port(5432)]
        }),
    ];
    for (name, set) in fields {
        let mut job = job.clone();
        set(&mut job);
        assert_eq!(
            build_layered(&job, &allocation, &ceiling, false),
            Err(Unsendable::RootfsNotReported),
            "{name}"
        );
        assert!(
            build_layered(&job, &allocation, &ceiling, true).is_ok(),
            "{name}"
        );
    }
    assert_eq!(
        Unsendable::RootfsNotReported.to_string(),
        "the job names layers, mounts or host ports and the host does not report layered-rootfs"
    );
}

/// A job with no layers, mounts or host ports starts with three empty lists, and is sent to
/// a host that does not report `layered-rootfs` with the start's JSON carrying none of
/// `layers`, `mounts` or `host_ports`, as before the fields. It catches a start that gains
/// keys an older agent reads as unknown, and a plain job refused for a capability it does not
/// need.
#[test]
fn a_job_without_rootfs_fields_starts_on_any_host_with_none_of_their_keys() {
    let (job, allocation) = records();
    for layered_rootfs in [false, true] {
        let start = build_layered(&job, &allocation, &allow_list(), layered_rootfs).unwrap();
        assert!(start.layers.is_empty() & start.mounts.is_empty() & start.host_ports.is_empty());
        let json = serde_json::to_value(&start).unwrap();
        let keys = json.as_object().unwrap();
        for key in ["layers", "mounts", "host_ports"] {
            assert!(!keys.contains_key(key), "{key} with {layered_rootfs}");
        }
    }
}

/// A job of layers alone (no image) starts with an empty image and its absolute command, the
/// command then its arguments. It catches an image invented for a job without one, and a
/// start refused for the empty image the layers stand in for.
#[test]
fn a_job_of_layers_alone_starts_with_an_empty_image() {
    let (mut job, allocation) = records();
    job.spec.image = None;
    job.spec.layers = vec![host_layer("/opt/rootfs")];
    job.spec.command = vec!["/bin/run".to_owned()];
    let start = build_layered(&job, &allocation, &allow_list(), true).unwrap();
    assert_eq!(start.image, "");
    assert_eq!(start.command, ["/bin/run", "--fast"]);
    assert_eq!(start.layers, [host_layer("/opt/rootfs")]);
}

/// A record whose layer the wire refuses (a host path not in normal form) is not sent: the
/// start is unsendable naming the wire's code and field, as the agent would refuse it. It
/// catches a stored job whose layer is passed to an agent unchecked.
#[test]
fn a_layer_the_wire_refuses_is_unsendable_naming_the_code_and_field() {
    let (mut job, allocation) = records();
    job.spec.layers = vec![host_layer("/opt/../tools")];
    assert_eq!(
        build_layered(&job, &allocation, &allow_list(), true),
        Err(Unsendable::Invalid {
            code: "invalid-job",
            field: "layers.path",
        })
    );
}
