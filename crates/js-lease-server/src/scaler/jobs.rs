//! The job tables in the scaler's words: what waits for a host, which hosts are in use, and
//! the queued jobs set aside for the machines launched for them.
//!
//! - **Demand** ([`demand`]). Each queued job that holds no live allocation is one waiting
//!   request, in queue order. A job that books a share of a host asks for its resources,
//!   disk, accelerators, platform, ISA level, execution mode, network level and host ports,
//!   as admission read them when it was submitted. A job that books the whole host asks for
//!   the pool's floor ([`Effects::whole_host`](super::Effects::whole_host), the least every
//!   host of the pool brings) with its own accelerator keys, platform, ISA level, execution
//!   mode, network level and ports, so one machine of the pool's shape is launched for it, and a host that holds
//!   any allocation does not cover it.
//! - **Busy** ([`in_use`]). A host is in use when a live (booked or releasing) allocation is
//!   on it; a lease the registry books on it does not count.
//! - **Reservation** ([`Reservations`]). When a step launches a machine for a queued job
//!   ([`js_scaler::State::launched`]), the job is set aside for that machine: from the next
//!   step on its request names the machine ([`js_scaler::Waiting::reserved`]), so the step
//!   neither launches for the job again nor counts the machine for another job, and once the
//!   machine's host is registered it is not drained as idle while it waits for the engine to
//!   place the job. The reservation is let go when the job leaves the queue (it was placed,
//!   it ended, or its row is gone), when the machine leaves the phases of a machine on its way
//!   and of a registered one (refused, failed, draining, releasing, gone or forgotten), when
//!   the machine was condemned, and when its registered host can no longer serve the job: it
//!   is not one the engine would place a job on now (not heard from, not serving, tainted,
//!   marked, cordoned or still clearing its last lease), another allocation is on it, or its
//!   announced shape does not fit the job. The job is then demand as any other. Reservations are kept in
//!   memory only: after a restart a queued job is matched again to a machine on its way by
//!   what it fits, as before any reservation.

use std::collections::{BTreeMap, BTreeSet};

use js_elastic::Demand;
use js_jobs::{AllocationRecord, JobId, JobRecord, Needs};
use js_scaler::{HostFacts, Phase, Records, Waiting};
use js_types::{AcceleratorNeed, HostId, RequestId, Resources};
use sha2::{Digest, Sha256};

use crate::job_api::validate::ISA_LEVEL_KEY;
use crate::job_start::job_network;

/// The phases of a machine on its way: it is held for the job it was launched for until its
/// host says hello.
const COMING: [Phase; 4] = [
    Phase::Planned,
    Phase::Launching,
    Phase::Unresolved,
    Phase::Booting,
];

/// How many hex digits of the job id's digest name a job whose id is no request id.
const DIGEST_DIGITS: usize = 40;

/// The request id the scaler knows the job `id` by: the job id itself when it is a request
/// id (lowercase letters, digits, `.`, `_` and `-`), and otherwise `job.` and the first
/// [`DIGEST_DIGITS`] hex digits of its SHA-256, the same at every step. `None` only if
/// neither is a request id, which no job id makes.
pub(super) fn request_id(id: &JobId) -> Option<RequestId> {
    let digested = |_| {
        let digest = Sha256::digest(id.as_str().as_bytes());
        let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        RequestId::new(&format!("job.{}", &hex[..DIGEST_DIGITS]))
    };
    RequestId::new(id.as_str()).or_else(digested).ok()
}

/// The hosts a live allocation of `live` is on, a whole-host one included.
pub(super) fn in_use(live: &[AllocationRecord]) -> BTreeSet<HostId> {
    live.iter()
        .map(|allocation| allocation.host.clone())
        .collect()
}

/// What the queued `job` asks of a host, with `whole_host` what a job that books the whole
/// host asks besides its constraints (the module docs). The accelerator of a share job is
/// its `accelerator.*` capabilities over its booked GPUs, as admission read it; a stored job
/// whose keys no longer read asks for its booked GPUs. The accelerator of a whole-host job is
/// its `accelerator.*` capabilities over the floor's devices; where they do not go with the
/// floor's (another model), they alone, as admission read them, so no machine of the floor is
/// launched for a job that asks for other devices; and where they do not read, the floor's.
pub(super) fn demand(job: &JobRecord, whole_host: &Demand) -> Demand {
    let spec = &job.spec;
    let constraints = &spec.constraints;
    let capabilities = &constraints.capabilities;
    let base = match &spec.needs {
        Needs::Shape(resources) => Demand {
            resources: resources.clone(),
            disk_bytes: Some(resources.disk_bytes()).filter(|bytes| *bytes > 0),
            accelerator: AcceleratorNeed::requested(capabilities, resources)
                .unwrap_or_else(|_| AcceleratorNeed::booked(resources)),
            ..whole_host.clone()
        },
        Needs::WholeHost => Demand {
            accelerator: AcceleratorNeed::requested(capabilities, &whole_host.resources)
                .or_else(|_| AcceleratorNeed::requested(capabilities, &Resources::default()))
                .unwrap_or_else(|_| whole_host.accelerator.clone()),
            ..whole_host.clone()
        },
    };
    let ports: BTreeSet<u16> = spec.ports.iter().map(|port| port.host).collect();
    let isa_level = capabilities
        .get(ISA_LEVEL_KEY)
        .and_then(|level| level.parse().ok());
    Demand {
        os: constraints.os.clone().or(base.os),
        arch: constraints.arch.clone().or(base.arch),
        isa_level: isa_level.or(base.isa_level),
        exec: constraints.exec.or(base.exec),
        network: job_network(spec.network),
        host_ports: ports.into_iter().collect(),
        ..base
    }
}

/// A queued job as the step reads it: its id, its request id and what it asks of a host.
pub(super) struct Queued {
    /// The job.
    pub(super) job: JobId,
    /// What the step calls it.
    pub(super) request: RequestId,
    /// What it asks of a host.
    pub(super) demand: Demand,
}

/// The queued jobs set aside for the machines launched for them, by job, each with the
/// machine's name (the module docs).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Reservations {
    held: BTreeMap<JobId, String>,
}

impl Reservations {
    /// Lets go of every reservation that no longer holds (the module docs), reading the
    /// queued jobs `queued`, the scaler's `records` and the registry's `hosts` as the step
    /// will see them, with `open` the hosts the registry would let take a job now (heard
    /// from, serving, untainted, unmarked, not clearing its last lease and not cordoned:
    /// those the engine places on).
    pub(super) fn settle(
        &mut self,
        queued: &[Queued],
        records: &Records,
        (hosts, open): (&[HostFacts], &BTreeSet<HostId>),
    ) {
        let waiting: BTreeMap<&JobId, &Demand> = queued
            .iter()
            .map(|queued| (&queued.job, &queued.demand))
            .collect();
        self.held.retain(|job, name| {
            let record = records.instances.iter().find(|each| each.name == *name);
            let host = hosts.iter().find(|host| host.id.as_str() == name.as_str());
            let serves = |demand: &&Demand| {
                host.is_some_and(|host| {
                    let fits = host
                        .shape
                        .as_ref()
                        .is_some_and(|shape| shape.fit(demand).is_ok());
                    [open.contains(&host.id), !host.busy, fits]
                        .into_iter()
                        .all(|ok| ok)
                })
            };
            let ready = waiting.get(job).is_some_and(serves);
            record.is_some_and(|record| {
                let registered = [record.phase == Phase::Registered, !record.condemned, ready];
                let coming = COMING.contains(&record.phase);
                [coming, registered.into_iter().all(|ok| ok)]
                    .into_iter()
                    .any(|ok| ok)
            }) & waiting.contains_key(job)
        });
    }

    /// Sets aside for its machine each job a step launched one for: `launched` names each
    /// new machine with its request, and `jobs` names the job of each request of the step.
    pub(super) fn note(
        &mut self,
        launched: &BTreeMap<String, RequestId>,
        jobs: &BTreeMap<RequestId, JobId>,
    ) {
        for (name, request) in launched {
            let job = jobs.get(request);
            self.held.extend(job.map(|job| (job.clone(), name.clone())));
        }
    }

    /// The waiting requests of `queued`, in order, each naming the machine its job is set
    /// aside for, if it is.
    pub(super) fn waiting(&self, queued: Vec<Queued>) -> Vec<Waiting> {
        let named = |job: &JobId| {
            let name = self.held.get(job);
            name.and_then(|name| HostId::new(name).ok())
        };
        queued
            .into_iter()
            .map(|queued| Waiting {
                reserved: named(&queued.job),
                id: queued.request,
                demand: queued.demand,
            })
            .collect()
    }
}

#[cfg(test)]
#[path = "jobs_tests.rs"]
mod tests;
