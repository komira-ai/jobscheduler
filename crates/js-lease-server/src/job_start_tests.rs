//! The start a placement is sent, and every reason one is not.

use std::collections::BTreeMap;

use js_agent_wire::{
    BookedResources, EGRESS_CAPABILITY, EGRESS_PORTS_CAPABILITY, NetworkCeiling, RunFile,
};
use js_jobs::{AllocationRecord, Booked, DeclaredPort, JobId, JobRecord, NetworkPolicy};
use js_types::{JobNetwork, PortMap};

use super::{Unsendable, start_of};
use crate::agent_api::job_rig::{BOOKED, image, placed, spec, tables};

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
    job_spec.files = BTreeMap::from([("upload".to_owned(), b"https://store/put".to_vec())]);
    placed(&jobs, "job-a", "host-a", job_spec, BOOKED)
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
    let start = start_of(&job, &allocation, &allow_list()).unwrap();
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
    assert_eq!(start.network, JobNetwork::AllowList);
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
        level: JobNetwork::Open,
        ports: Vec::new(),
    };
    let start = start_of(&open, &allocation, &ceiling).unwrap();
    assert_eq!(start.network, JobNetwork::Open);
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

/// Each start the wire cannot carry is refused with its reason: a whole-host booking, GPUs
/// booked by count, a network level or a port above the host's ceiling, a file that is not
/// text, a wall limit that overflows, and a job id the wire's grammar refuses. It catches a
/// start sent with something the host would refuse or misread.
#[test]
fn a_start_the_wire_cannot_carry_says_why() {
    let (job, allocation) = records();
    let ceiling = allow_list();
    let mut whole = allocation.clone();
    whole.booked = Booked::WholeHost;
    assert_eq!(start_of(&job, &whole, &ceiling), Err(Unsendable::WholeHost));
    let mut gpus = allocation.clone();
    gpus.booked = Booked::Shape {
        cpu_millis: 1000,
        memory_bytes: 1 << 30,
        disk_bytes: 0,
        accelerators: 2,
    };
    assert_eq!(
        start_of(&job, &gpus, &ceiling),
        Err(Unsendable::GpuIndices(2))
    );
    assert_eq!(
        start_of(&job, &allocation, &NetworkCeiling::default()),
        Err(Unsendable::AboveCeiling)
    );
    let mut unforwarded = job.clone();
    unforwarded.spec.ports[0].host = 9090;
    assert_eq!(
        start_of(&unforwarded, &allocation, &ceiling),
        Err(Unsendable::AboveCeiling)
    );
    let mut binary = job.clone();
    binary
        .spec
        .files
        .insert("blob".to_owned(), vec![0xff, 0xfe]);
    assert_eq!(
        start_of(&binary, &allocation, &ceiling),
        Err(Unsendable::FileNotText("blob".to_owned()))
    );
    let mut endless = job.clone();
    endless.spec.wall_limit_secs = u64::MAX;
    assert_eq!(
        start_of(&endless, &allocation, &ceiling),
        Err(Unsendable::WallLimit)
    );
    let mut dashed = job;
    dashed.id = JobId::parse("-job").unwrap();
    assert_eq!(
        start_of(&dashed, &allocation, &ceiling),
        Err(Unsendable::Invalid {
            code: "invalid-job",
            field: "job_id",
        })
    );
}
