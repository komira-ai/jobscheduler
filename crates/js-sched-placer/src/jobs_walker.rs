//! A queued job as a walk offers it to the engine ([`Walker`]): its request, what it books,
//! the hosts its bars keep it off and what else the walk needs of it.

use std::collections::BTreeSet;
use std::time::Duration;

use js_jobs::{Booked, Isolation, JobId, JobRecord, Needs};
use js_sched::{Prediction, Request};
use js_types::sched::FarmTime;
use js_types::{HostId, JobNetwork};
use tracing::warn;

use super::curve_room::Shaped;
use super::jobs::{job_network, job_request};
use super::whole_jobs;

/// A queued job, as a pass offers it to the engine.
pub(super) struct Walker {
    pub(super) job: JobId,
    /// Whether the job runs on the machine itself (host isolation).
    pub(super) host_isolation: bool,
    /// The hosts its bars keep it off at the walk's time ([`whole_jobs::barred_hosts`]).
    pub(super) barred: BTreeSet<HostId>,
    pub(super) request: Request,
    pub(super) booked: Booked,
    /// The network level the job asks for.
    pub(super) network: JobNetwork,
    /// The host ports the job declares, sorted, each once.
    pub(super) host_ports: Vec<u16>,
    pub(super) wall: Duration,
    pub(super) charge: Prediction,
    /// The CPU curve the job books by when it leaves its CPU at 0 and its class passes its
    /// checks under `--placement-curves on` ([`super::Engine::shaped`]); `None`: it books its
    /// flat number at every slot. The booking it is stored with is its flat number either way.
    pub(super) curve: Option<Shaped>,
}

/// The queued `job` as a pass offers it to the engine at `now`; `None` for a job that books the
/// whole host (the pass queues it with [`super::Engine::whole_jobs`]), and for one whose constraints
/// are no platform requirements (logged).
pub(super) fn walker(job: &JobRecord, now: FarmTime) -> Option<Walker> {
    let Needs::Shape(resources) = &job.spec.needs else {
        return None;
    };
    let request = job_request(job, resources)
        .inspect_err(|error| warn!(job = %job.id, %error, "a queued job's constraints are no platform requirements; it is not placed"))
        .ok()?;
    let ports: BTreeSet<u16> = job.spec.ports.iter().map(|port| port.host).collect();
    let charge = Prediction {
        cpu_millis: resources.cpu_millis(),
        memory_bytes: resources.memory_bytes(),
        cold: false,
        life_minutes: 1,
    };
    Some(Walker {
        job: job.id.clone(),
        host_isolation: job.spec.constraints.isolation == Isolation::Host,
        barred: whole_jobs::barred_hosts(job, now),
        request,
        booked: Booked::Shape {
            cpu_millis: resources.cpu_millis(),
            memory_bytes: resources.memory_bytes(),
            disk_bytes: resources.disk_bytes(),
            accelerators: resources.gpus(),
        },
        network: job_network(job.spec.network),
        host_ports: ports.into_iter().collect(),
        wall: Duration::from_secs(job.spec.wall_limit_secs),
        charge,
        curve: None,
    })
}
