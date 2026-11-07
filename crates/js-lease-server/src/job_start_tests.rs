//! The start a placement is sent, and every reason one is not.

use std::collections::BTreeMap;

use js_agent_wire::{
    BookedResources, EGRESS_CAPABILITY, EGRESS_PORTS_CAPABILITY, Isolation, NetworkCeiling, RunFile,
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
