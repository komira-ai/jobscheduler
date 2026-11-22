//! The start a placement is sent, and every reason one is not.

use std::collections::BTreeMap;
use std::time::Duration;

use js_agent_wire::{
    BookedFrom, BookedResources, EGRESS_CAPABILITY, EGRESS_PORTS_CAPABILITY, HostPort, Isolation,
    JobMount, Layer, MountMode, MountSource, NetworkCeiling, NetworkLevel, PortProtocol,
    Propagation, RunFile,
};
use js_jobs::{
    AllocationRecord, Booked, DeclaredPort, JobFiles, JobId, JobRecord, Needs, NetworkPolicy,
    SizedFrom,
};
use js_state::StoreTime;
use js_types::{
    Arch, ExecMode, HostId, ImageDigestRef, JobNetwork as Level, Os, PortMap, Resources,
};

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
            polls_dispatch: false,
            metrics: &NoMetrics,
            layered_rootfs: false,
            mount_providers: &[],
            xcodes: &[],
            limits: crate::job_expiry::JobLimits::DEFAULT,
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

/// The start carries the Unix time in milliseconds at which the job was placed, from the
/// record's `placed_at` exactly (a fraction of a millisecond is dropped), and none for a
/// record that has no placement time. It catches a placement time carried in seconds or
/// nanoseconds, or one made up when the record has none.
#[test]
fn a_start_carries_the_time_the_job_was_placed_in_milliseconds() {
    let (mut job, allocation) = records();
    job.placed_at = Some(StoreTime::from_duration(Duration::from_micros(
        1_800_000_123_456_789,
    )));
    let start = build(&job, &allocation, &allow_list()).unwrap();
    assert_eq!(start.placed_at_ms, Some(1_800_000_123_456));

    job.placed_at = None;
    let start = build(&job, &allocation, &allow_list()).unwrap();
    assert_eq!(start.placed_at_ms, None);
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
            polls_dispatch: false,
            metrics: &NoMetrics,
            layered_rootfs,
            mount_providers: &[],
            xcodes: &[],
            limits: crate::job_expiry::JobLimits::DEFAULT,
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

/// The start of `job` for `allocation` to `host-a`, whose agent reports `layered-rootfs` and,
/// as `mount-provider.<name>` capabilities, the keys `reported`.
fn build_providers(
    job: &JobRecord,
    allocation: &AllocationRecord,
    reported: &[String],
) -> Result<js_agent_wire::JobStartSpec, Unsendable> {
    let host = HostId::new("host-a").unwrap();
    start_of(
        job,
        allocation,
        &BeatContext {
            host: &host,
            ceiling: &allow_list(),
            host_jobs: false,
            shape: None,
            logs: None,
            polls_dispatch: false,
            metrics: &NoMetrics,
            layered_rootfs: true,
            mount_providers: reported,
            xcodes: &[],
            limits: crate::job_expiry::JobLimits::DEFAULT,
        },
    )
}

/// A job that requires the capabilities of the providers `names`, as a submit records them,
/// with a provider layer of each.
fn provided(job: &JobRecord, names: &[&str]) -> JobRecord {
    let mut job = job.clone();
    for name in names {
        job.spec
            .constraints
            .capabilities
            .insert(js_caps::mount_provider_key(name), "true".to_owned());
        job.spec.layers.push(Layer::Provider {
            provider: (*name).to_owned(),
            reference: "trees/0123".to_owned(),
        });
    }
    job
}

/// A job whose layers or mounts name providers is not sent to a host whose agent did not report
/// each `mount-provider.<name>` the job requires, with the text the log and the start timeout
/// show, whatever else the host reports (a different provider, `layered-rootfs`); sent to one
/// that reports them all, with the provider layers as the record holds them. A job that requires
/// no provider is sent to a host that reports none. It catches a start that an agent without
/// the provider would fail, or that one from before provider sources would not read, and a
/// host that reports one provider of two taken as enough.
#[test]
fn a_job_requiring_a_provider_is_not_sent_to_a_host_that_does_not_report_it() {
    let (plain, allocation) = records();
    let both = provided(&plain, &["cache", "tierstore"]);
    let key = js_caps::mount_provider_key;
    assert_eq!(
        build_providers(&both, &allocation, &[]),
        Err(Unsendable::MountProviderNotReported(key("cache")))
    );
    assert_eq!(
        build_providers(&both, &allocation, &[key("other"), key("tierstore")]),
        Err(Unsendable::MountProviderNotReported(key("cache")))
    );
    assert_eq!(
        build_providers(&both, &allocation, &[key("cache")]),
        Err(Unsendable::MountProviderNotReported(key("tierstore")))
    );
    let start = build_providers(&both, &allocation, &[key("cache"), key("tierstore")]).unwrap();
    assert_eq!(
        start.layers,
        [
            Layer::Provider {
                provider: "cache".to_owned(),
                reference: "trees/0123".to_owned(),
            },
            Layer::Provider {
                provider: "tierstore".to_owned(),
                reference: "trees/0123".to_owned(),
            },
        ]
    );
    assert!(build_providers(&plain, &allocation, &[]).is_ok());
    assert!(build_providers(&plain, &allocation, &[key("tierstore")]).is_ok());
    assert_eq!(
        Unsendable::MountProviderNotReported(key("cache")).to_string(),
        "the job requires mount-provider.cache and the host does not report it"
    );
}

/// The start of `job` for `allocation` to `host-a`, whose agent lists the Xcode builds
/// `reported`.
fn build_xcodes(
    job: &JobRecord,
    allocation: &AllocationRecord,
    reported: &[String],
) -> Result<js_agent_wire::JobStartSpec, Unsendable> {
    let host = HostId::new("host-a").unwrap();
    start_of(
        job,
        allocation,
        &BeatContext {
            host: &host,
            ceiling: &allow_list(),
            host_jobs: false,
            shape: None,
            logs: None,
            metrics: &NoMetrics,
            layered_rootfs: false,
            mount_providers: &[],
            xcodes: reported,
            polls_dispatch: false,
            limits: crate::job_expiry::JobLimits::DEFAULT,
        },
    )
}

/// A job that names the Xcode build `build`, as a submit records it.
fn with_xcode(job: &JobRecord, build: &str) -> JobRecord {
    let mut job = job.clone();
    job.spec
        .constraints
        .capabilities
        .insert(js_caps::XCODE.to_owned(), build.to_owned());
    job
}

/// A job that names an Xcode build gets a start whose `xcode` is that build when the host's
/// agent lists it among its builds (the one of two it names, and the other), and one that names
/// none gets a start with no `xcode`, whatever the host lists. It catches a build dropped from
/// the start, which would run the job with the host's selected Xcode.
#[test]
fn a_start_carries_the_xcode_build_its_job_names() {
    let (plain, allocation) = records();
    let named = with_xcode(&plain, "16E140");
    let held = ["16C5032a".to_owned(), "16E140".to_owned()];
    let start = build_xcodes(&named, &allocation, &held).unwrap();
    assert_eq!(start.xcode.as_deref(), Some("16E140"));
    let start = build_xcodes(&with_xcode(&plain, "16C5032a"), &allocation, &held).unwrap();
    assert_eq!(start.xcode.as_deref(), Some("16C5032a"));
    assert_eq!(
        build_xcodes(&plain, &allocation, &held).unwrap().xcode,
        None
    );
    assert_eq!(build_xcodes(&plain, &allocation, &[]).unwrap().xcode, None);
}

/// A job that names an Xcode build is not sent to a host whose agent did not list it (no
/// Xcode at all, or only other builds), with the text the log and the start timeout show; a
/// build the wire refuses is unsendable naming the field. It catches a start an agent would
/// refuse, or one from before the field would run with the wrong Xcode.
#[test]
fn a_job_naming_an_xcode_is_not_sent_to_a_host_that_does_not_list_it() {
    let (plain, allocation) = records();
    let named = with_xcode(&plain, "16E140");
    let refused = Err(Unsendable::XcodeNotReported("16E140".to_owned()));
    assert_eq!(build_xcodes(&named, &allocation, &[]), refused);
    let others = ["16C5032a".to_owned(), "15F31d".to_owned()];
    assert_eq!(build_xcodes(&named, &allocation, &others), refused);
    assert_eq!(
        Unsendable::XcodeNotReported("16E140".to_owned()).to_string(),
        "the job requires Xcode 16E140 and the host does not report it"
    );
    let bad = with_xcode(&plain, "16E 140");
    assert_eq!(
        build_xcodes(&bad, &allocation, &["16E 140".to_owned()]),
        Err(Unsendable::Invalid {
            code: "invalid-job",
            field: "xcode",
        })
    );
}

/// The digest of the bundle images of the native tests.
fn bundle_digest() -> String {
    "b".repeat(64)
}

/// A job as kbf submits a macOS task: a bundle image named `image_name`, no command, the task
/// flags as arguments, no network, `os=macos arch=arm64 exec=sandbox-exec`, and a read-only
/// and a read-write host-path mount, both `no_exec`.
fn native_records(image_name: &str) -> (JobRecord, AllocationRecord) {
    let (mut job, allocation) = records();
    job.spec.image =
        ImageDigestRef::parse(&format!("{image_name}@sha256:{}", bundle_digest())).ok();
    job.spec.command = Vec::new();
    job.spec.args = vec!["--task".to_owned(), "build".to_owned()];
    job.spec.network = NetworkPolicy::None;
    job.spec.ports = Vec::new();
    job.spec.constraints.os = Some("macos".to_owned());
    job.spec.constraints.arch = Some("arm64".to_owned());
    job.spec.constraints.exec = Some(ExecMode::SandboxExec);
    job.spec.mounts = vec![
        JobMount {
            no_exec: true,
            ..mount_of("/Users/ci/tree", "/work/.kbf-tree")
        },
        JobMount {
            no_exec: true,
            mode: MountMode::ReadWrite,
            ..mount_of("/Users/ci/out", "/work/.kbf-out")
        },
    ];
    (job, allocation)
}

/// The start of a native job, to a host that reports `layered-rootfs`.
fn native_start(job: &JobRecord, allocation: &AllocationRecord) -> js_agent_wire::JobStartSpec {
    build_layered(job, allocation, &allow_list(), true).unwrap()
}

/// A job that asks for `sandbox-exec` with an image and no command starts in the agent's
/// one-job form: the program is the bundle's default entrypoint (named as the bundle, the
/// last part of the image name), the run's arguments are the job's, the command is empty, the
/// system and architecture are parsed, the network is off, the environment stays in `env`, the
/// image keeps its digest and the mounts are carried. It catches a native job sent as a
/// container command, or one that loses its mounts, its system or its environment.
#[test]
fn a_native_job_without_a_command_runs_the_bundle_named_entrypoint_with_its_args() {
    let (job, allocation) = native_records("registry.example/team/kbf-task");
    let start = native_start(&job, &allocation);
    let run = start.run.as_ref().unwrap();
    assert_eq!(run.program, "@bundle/kbf-task");
    assert_eq!(run.args, ["--task", "build"]);
    assert!(run.env.is_empty() && run.inputs.is_empty() && run.outputs.is_empty());
    assert_eq!(run.cwd, "");
    assert_eq!(run.network, NetworkLevel::Off);
    assert!(start.command.is_empty());
    assert_eq!(start.os, Some(Os::Macos));
    assert_eq!(start.arch, Some(Arch::Arm64));
    assert_eq!(start.network, Level::None);
    assert_eq!(start.env, job.spec.env);
    assert_eq!(
        start.image,
        format!("registry.example/team/kbf-task@sha256:{}", bundle_digest())
    );
    assert_eq!(start.mounts, job.spec.mounts);
}

/// The default entrypoint is the bundle's name without a tag, and a name with no `/` is the
/// bundle's name whole. It catches a tag or a registry path leaking into the program.
#[test]
fn the_default_entrypoint_drops_the_registry_path_and_the_tag() {
    for (name, program) in [
        ("kbf-task", "@bundle/kbf-task"),
        ("kbf-task:v1", "@bundle/kbf-task"),
        ("host:5000/team/kbf-task:v1", "@bundle/kbf-task"),
        ("host:5000/kbf-task", "@bundle/kbf-task"),
    ] {
        let (job, allocation) = native_records(name);
        let start = native_start(&job, &allocation);
        assert_eq!(start.run.unwrap().program, program, "{name}");
    }
}

/// A command whose first entry is a bare name names an entrypoint of the bundle, one that
/// starts with `@bundle/` or `/` is kept as written, and the rest of the command comes before
/// the job's arguments. It catches a prefix added twice, an absolute path turned into an
/// entrypoint, or the command's tail dropped or put after the arguments.
#[test]
fn a_native_command_names_an_entrypoint_a_bundle_path_or_an_absolute_program() {
    for (first, program) in [
        ("tool", "@bundle/tool"),
        ("@bundle/other", "@bundle/other"),
        ("/usr/bin/env", "/usr/bin/env"),
    ] {
        let (mut job, allocation) = native_records("reg/kbf-task");
        job.spec.command = vec![first.to_owned(), "sub".to_owned()];
        let start = native_start(&job, &allocation);
        let run = start.run.unwrap();
        assert_eq!(run.program, program, "{first}");
        assert_eq!(run.args, ["sub", "--task", "build"], "{first}");
        assert!(start.command.is_empty(), "{first}");
    }
}

/// The run's network follows the job's: none is off, an allow-list is on, open is open, and
/// the start's own network keeps the job's level. It catches a mapping that widens or narrows
/// a level.
#[test]
fn a_native_runs_network_follows_the_jobs_network() {
    let ceiling = NetworkCeiling::from_capabilities(&BTreeMap::from([(
        EGRESS_CAPABILITY.to_owned(),
        "open".to_owned(),
    )]))
    .unwrap();
    for (policy, run, start_level) in [
        (NetworkPolicy::None, NetworkLevel::Off, Level::None),
        (NetworkPolicy::AllowList, NetworkLevel::On, Level::AllowList),
        (NetworkPolicy::Open, NetworkLevel::Open, Level::Open),
    ] {
        let (mut job, allocation) = native_records("reg/kbf-task");
        job.spec.network = policy;
        let start = build_layered(&job, &allocation, &ceiling, true).unwrap();
        assert_eq!(start.run.unwrap().network, run);
        assert_eq!(start.network, start_level);
    }
}

/// A constraint that names no known system or architecture leaves the start's field out, and
/// `aarch64` reads as `arm64`. It catches a start refused, or a guessed value sent, for an
/// unknown spelling.
#[test]
fn a_native_start_leaves_out_a_system_or_architecture_it_cannot_parse() {
    let (mut job, allocation) = native_records("reg/kbf-task");
    job.spec.constraints.os = Some("plan9".to_owned());
    job.spec.constraints.arch = Some("aarch64".to_owned());
    let start = native_start(&job, &allocation);
    assert_eq!((start.os, start.arch), (None, Some(Arch::Arm64)));
    job.spec.constraints.os = None;
    job.spec.constraints.arch = Some("mips".to_owned());
    let start = native_start(&job, &allocation);
    assert_eq!((start.os, start.arch), (None, None));
}

/// A job that is not native is built as before: with `exec` unspecified or `container` it has
/// no run, no system and no architecture, and its command then its arguments are the start's
/// command, though its constraints name a system. A `sandbox-exec` job with no image (layers
/// alone) is not a bundle and is built the same way. It catches the native form applied to a
/// container job.
#[test]
fn a_job_that_is_not_native_has_no_run_and_its_command_then_args() {
    for exec in [None, Some(ExecMode::Container)] {
        let (mut job, allocation) = native_records("reg/kbf-task");
        job.spec.command = vec!["tool".to_owned()];
        job.spec.constraints.exec = exec;
        let start = native_start(&job, &allocation);
        assert_eq!(start.run, None, "{exec:?}");
        assert_eq!(start.command, ["tool", "--task", "build"], "{exec:?}");
        assert_eq!((start.os, start.arch), (None, None), "{exec:?}");
    }
    let (mut job, allocation) = native_records("reg/kbf-task");
    job.spec.image = None;
    job.spec.layers = vec![host_layer("/opt/rootfs")];
    job.spec.command = vec!["/bin/run".to_owned()];
    let start = native_start(&job, &allocation);
    assert_eq!(start.run, None);
    assert_eq!(start.command, ["/bin/run", "--task", "build"]);
}

/// A native job whose program the wire's entrypoint grammar refuses (a space in the name) is
/// unsendable naming the wire's code and field. It catches a program passed to an agent
/// unchecked.
#[test]
fn a_native_program_the_wire_refuses_is_unsendable() {
    let (mut job, allocation) = native_records("reg/kbf-task");
    job.spec.command = vec!["bad name".to_owned()];
    assert_eq!(
        build_layered(&job, &allocation, &allow_list(), true),
        Err(Unsendable::Invalid {
            code: "invalid-program",
            field: "run.program",
        })
    );
}

/// A placed job asking `cpu_millis` and `memory_bytes` (zero: unnamed), and its allocation as
/// `sized_from` sized it.
fn sized(
    (cpu_millis, memory_bytes): (u64, u64),
    sized_from: Option<SizedFrom>,
) -> (JobRecord, AllocationRecord) {
    let (mut job, mut allocation) = records();
    job.spec.needs = Needs::Shape(Resources::new(cpu_millis, memory_bytes, 0, None).unwrap());
    allocation.sized_from = sized_from;
    (job, allocation)
}

/// The cpu and memory sources of the sizing of `job` for `allocation`.
fn sources(job: &JobRecord, allocation: &AllocationRecord) -> (BookedFrom, BookedFrom) {
    let sizing = super::sizing_of(job, allocation).expect("a share carries a sizing");
    (sizing.cpu, sizing.memory)
}

/// A job that names both its CPU and its memory has both from the job, whatever the
/// allocation records. It catches a named dimension credited to a history.
#[test]
fn a_job_that_names_both_has_both_named() {
    let (job, allocation) = sized((2000, 1 << 30), Some(SizedFrom::Cold));
    assert_eq!(
        sources(&job, &allocation),
        (BookedFrom::Named, BookedFrom::Named)
    );
}

/// A job that names its CPU only has its memory from the history that sized the allocation,
/// each history as itself. It catches a mapping that swaps two histories or credits the memory
/// the job did not name to the job.
#[test]
fn a_job_that_names_its_cpu_has_its_memory_from_the_history() {
    for (from, want) in [
        (SizedFrom::Class, BookedFrom::Class),
        (SizedFrom::Fallback, BookedFrom::Fallback),
        (SizedFrom::Cold, BookedFrom::Cold),
        (SizedFrom::OverCap, BookedFrom::OverCap),
    ] {
        let (job, allocation) = sized((2000, 0), Some(from));
        assert_eq!(sources(&job, &allocation), (BookedFrom::Named, want));
    }
}

/// A job that names its memory only has its CPU from the history. It catches the two
/// dimensions read from each other's field.
#[test]
fn a_job_that_names_its_memory_has_its_cpu_from_the_history() {
    let (job, allocation) = sized((0, 1 << 30), Some(SizedFrom::Class));
    assert_eq!(
        sources(&job, &allocation),
        (BookedFrom::Class, BookedFrom::Named)
    );
}

/// A job that names neither, on an allocation that records no history (a row from before the
/// field), has both unknown. It catches a guess in place of the unknown.
#[test]
fn a_job_that_names_neither_on_an_allocation_without_history_is_unknown() {
    let (job, allocation) = sized((0, 0), None);
    assert_eq!(
        sources(&job, &allocation),
        (BookedFrom::Unknown, BookedFrom::Unknown)
    );
}

/// A job that books the whole host, by its needs or by its allocation, sends no sizing. It
/// catches a sizing sent for a booking no history sized.
#[test]
fn a_whole_host_job_sends_no_sizing() {
    let (mut job, allocation) = sized((0, 0), Some(SizedFrom::Cold));
    job.spec.needs = Needs::WholeHost;
    assert_eq!(super::sizing_of(&job, &allocation), None);
    let (job, mut allocation) = sized((0, 0), None);
    allocation.booked = Booked::WholeHost;
    assert_eq!(super::sizing_of(&job, &allocation), None);
}

/// The sizing names the job's size-class label when it has one, else the class the placement
/// keys its runs under; and the start carries it. It catches a class that is not the key the
/// history is learned under.
#[test]
fn the_sizing_names_the_jobs_size_class() {
    let (mut job, allocation) = sized((0, 0), Some(SizedFrom::Class));
    let labels = BTreeMap::from([("size-class".to_owned(), "compile".to_owned())]);
    job.spec.labels = js_types::Labels::from_caller(&labels).unwrap();
    let sizing = super::sizing_of(&job, &allocation).unwrap();
    assert_eq!(sizing.class.as_deref(), Some("compile"));
    let start = build(&job, &allocation, &allow_list()).unwrap();
    assert_eq!(start.sizing, Some(sizing));

    job.spec.labels = js_types::Labels::default();
    let unlabelled = super::sizing_of(&job, &allocation).unwrap();
    assert_eq!(
        unlabelled.class,
        Some(js_sched_placer::sizes::class_name(&job))
    );
    assert!(unlabelled.class.unwrap().starts_with("run-"));
}
