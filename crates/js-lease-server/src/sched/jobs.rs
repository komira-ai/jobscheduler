//! The queued jobs placed by the engine, and the job allocations it books (design sections
//! 5.3, steps 5 and 6, 6.2 and 6.7, and section 8, rows 11, 12, 17 and 25).
//!
//! Under `--placement js-sched`, with the job tables, the server runs a **pass** over the
//! queue on each tick and after each heartbeat of a host that speaks the per-job messages
//! ([`Agents::place_jobs`]): a job end, a cancel, a confirmed container, a host that joins
//! and a heartbeat that changes a host's load all come to the server as one of those. A pass
//! ([`SchedPlacer::place_jobs`]):
//!
//! 1. **Reads the tables as they stand**: every live (booked or releasing) allocation and
//!    every queued job in queue order. The engine holds no list of its own that a job must
//!    stay on, so no timer can drop a queued job and none is ever presented again (row 12).
//! 2. **Brings the engine's bookings to the allocation table** ([`Engine::sync_jobs`]): the
//!    allocation table is the one record of what a job books on a host: its resources and
//!    its host ports. A booking freed (by the host's confirmation or the provider's absence,
//!    [`crate::job_alloc`]) is let go, with its ports; a booking the engine does not hold
//!    (after a restart) is taken up before anything is placed, with the ports the table
//!    records for it, so it is never offered again and its job is never placed twice. Booked
//!    and releasing allocations both book, and both hold their ports until freed.
//! 3. **Walks the queue** ([`Engine::walk_jobs`]) in the binding order of
//!    [`js_sched::walk_in_order`], one placement round of the scheduler per job, over the
//!    hosts the registry would let take a job now ([`open_to_jobs`]), each with the network
//!    ceiling its hello advertises, read again on every pass. A host is capable of a job only
//!    if it can run it and its ceiling covers the job's network level and each host port the
//!    job declares ([`js_sched::ceiling::within_ceiling`]); a host that advertised nothing
//!    reads as level none and no port. The engine's free capacity on a host is what its
//!    shape leaves after the job allocations and the registry's lease bookings on it, which
//!    the engine holds side by side, so a job and a lease can never book the same room (row
//!    25). A host port is a resource of the allocation too: a host where an allocation the
//!    engine holds already has a port the job declares has no room for it, so one host never
//!    holds a port twice, and the job waits as for room. Each placement is held in the
//!    engine at once, with its ports, so a job or a lease placed while the pass commits does
//!    not take its room or its port.
//! 4. **Commits each placement** with [`js_jobs::JobStore::place`]: in one transaction the job
//!    becomes placed on the host and its allocation is written as booked, with the host ports the
//!    job declares. Only after the store reports the commit is the host's start handed on
//!    ([`JobStarts`]). A failed commit lets the engine's hold go ([`Engine::job_failed`]): nothing
//!    is booked, the job stays queued and the next pass places it.
//!
//! The start rides on the host's next heartbeat reply, built from the job table
//! ([`crate::per_job`]): so the server's [`JobStarts`] is [`NoStarts`], and a placement
//! whose start was never delivered (the server stopped after the commit) is started from the
//! table after a restart all the same.
//!
//! A job that books the whole host is not placed by a pass, nor by anything else yet, and
//! holds back no other job; a whole-host booking books the whole host in the scheduler, so no
//! job with a port is placed beside it and a request for the whole host waits for every
//! holder. A lease is never in the queue: it
//! is placed on arrival by the registry's placer, which sees the job allocations too.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use js_agent_wire::{NetworkCeiling, Phase};
use js_jobs::{AllocationRecord, Booked, JobId, JobRecord, JobStoreError, Needs};
use js_lease::{Clock, Timestamp};
use js_registry::{HostState, HostView};
use js_sched::ceiling::{ceiling_of, within_ceiling};
use js_sched::{Event, Prediction, Request};
use js_types::sched::{FarmTime, LeaseId as SchedLeaseId, LeaseKind, Qos, WorkKey};
use js_types::{HostId, JobNetwork, Resources};
use tracing::warn;

use super::super::map::{self, MapError};
use super::super::place::SchedPlacer;
use super::{Engine, Placed, recovered_as};
use crate::agents::Agents;
use crate::job_start::job_network;
use crate::job_store::ServerJobs;

/// The term of the leases the engine places for jobs: each is numbered by its operation, so
/// none is a lease of the registry's (term 1) or one taken up (term 0).
const JOB_TERM: u64 = 2;

/// A job allocation the engine holds.
#[derive(Debug, Clone)]
pub(crate) struct JobHold {
    /// Its host.
    host: HostId,
    /// What it books there.
    booked: Booked,
    /// The host ports it holds there, sorted, each once: the job's declared ports, booked with
    /// the allocation and let go with it.
    host_ports: Vec<u16>,
    /// The engine's lease and operation for it.
    placed: Placed,
}

/// A queued job, as a pass offers it to the engine.
struct Walker {
    job: JobId,
    request: Request,
    booked: Booked,
    /// The network level the job asks for.
    network: JobNetwork,
    /// The host ports the job declares, sorted, each once.
    host_ports: Vec<u16>,
    wall: Duration,
    charge: Prediction,
}

impl Engine {
    /// Whether the engine holds a job allocation on `host`.
    pub(crate) fn holds_jobs_on(&self, host: &HostId) -> bool {
        self.jobs.values().any(|hold| hold.host == *host)
    }

    /// The engine's leases of the job allocations it holds on `host`.
    pub(super) fn job_leases_on<'a>(
        &'a self,
        host: &'a HostId,
    ) -> impl Iterator<Item = SchedLeaseId> + 'a {
        let here = self.jobs.values().filter(move |hold| hold.host == *host);
        here.map(|hold| hold.placed.lease)
    }

    /// Whether the engine holds a job allocation on `host` that holds one of the host ports
    /// `ports`.
    fn port_held_on(&self, host: &HostId, ports: &[u16]) -> bool {
        let mut here = self.jobs.values().filter(|hold| hold.host == *host);
        here.any(|hold| hold.host_ports.iter().any(|port| ports.contains(port)))
    }

    /// Takes up each of `holds`, as an engine built again takes up what the one before it
    /// held of the job allocations.
    pub(super) fn take_jobs(&mut self, holds: &BTreeMap<JobId, JobHold>) {
        for (job, hold) in holds {
            self.take_job(job, &hold.host, hold.booked, hold.host_ports.clone());
        }
    }

    /// Takes up the allocation of `job` on `host`, booking `booked` and the host ports
    /// `host_ports`, as a lease recovered after a restart is: held there (on a host that has not
    /// said hello yet, until its first heartbeat lists it or the engine's grace runs out), and
    /// never placed again by the engine alone.
    fn take_job(&mut self, job: &JobId, host: &HostId, booked: Booked, host_ports: Vec<u16>) {
        let placed = Placed {
            lease: SchedLeaseId::new(0, self.taken),
            operation: self.sched.next_operation(),
        };
        self.taken = self.taken.saturating_add(1);
        let key = WorkKey::new("job", job.as_str());
        let _ = self
            .sched
            .take_in(recovered_as(placed, host, key, booking(booked)));
        self.hold_job(job, host, (booked, host_ports), placed);
    }

    /// Records that the engine holds the allocation of `job` on `host` as `placed`, booking
    /// `booked` (what it books of the host and its host ports), and holds its operation to
    /// no host.
    fn hold_job(&mut self, job: &JobId, host: &HostId, booked: (Booked, Vec<u16>), placed: Placed) {
        let (booked, host_ports) = booked;
        self.sched.place_only_on(placed.operation, BTreeSet::new());
        let hold = JobHold {
            host: host.clone(),
            booked,
            host_ports,
            placed,
        };
        self.jobs.insert(job.clone(), hold);
    }

    /// Brings the job allocations the engine holds to `live`, the live allocations of the
    /// job table (design section 6.2): one the table no longer holds live, or holds on
    /// another host, is let go; one the scheduler gave up (its host silent past the engine's
    /// grace) is taken up again, since silence frees nothing; one the engine does not hold
    /// (the table's after a restart) is taken up.
    pub(crate) fn sync_jobs(&mut self, live: &[AllocationRecord]) {
        let live: BTreeMap<&JobId, &AllocationRecord> = live
            .iter()
            .map(|allocation| (&allocation.job, allocation))
            .collect();
        let stale: Vec<JobId> = self
            .jobs
            .iter()
            .filter(|(job, hold)| {
                let listed = live.get(job).is_some_and(|live| live.host == hold.host);
                !listed | !self.still_held(hold)
            })
            .map(|(job, _)| job.clone())
            .collect();
        for job in &stale {
            self.job_failed(job);
        }
        let missing: Vec<&AllocationRecord> = live
            .into_values()
            .filter(|allocation| !self.jobs.contains_key(&allocation.job))
            .collect();
        for allocation in missing {
            let ports = allocation.host_ports.clone();
            self.take_job(&allocation.job, &allocation.host, allocation.booked, ports);
        }
    }

    /// Whether the scheduler still books `hold` on its host.
    fn still_held(&self, hold: &JobHold) -> bool {
        let worker = map::worker(&hold.host);
        self.sched.leases_on(&worker).contains(&hold.placed.lease)
    }

    /// Lets go of the allocation of `job`: its placement was not committed, or the table no
    /// longer holds it live. Its booking is free again. A job the engine does not hold
    /// changes nothing.
    pub(crate) fn job_failed(&mut self, job: &JobId) {
        let placed = self.jobs.remove(job).map(|hold| hold.placed);
        placed.into_iter().for_each(|placed| self.release(placed));
    }

    /// One walk of `queued`, the queued jobs in queue order, over `open`, the hosts that may take a
    /// job now with the network ceiling each advertises, at `now`: the placements it decides, each
    /// as the job, its host and what it books, in queue order. Each is held in the engine from here
    /// on, so it books its host for whatever is placed after it, until its commit answers.
    ///
    /// Each open host the engine knows by a shape it can describe is heard from now, with its
    /// capacity, as at a lease's start; the others are left out. The walk is
    /// [`js_sched::walk_in_order`]: each job is placed by one placement round held to the hosts the
    /// walk leaves open to it (the same load placement, CPU slack and memory percent as a lease),
    /// and a job placed nowhere closes to the jobs behind it every host it could run on with room
    /// (its capabilities match and it fits the host's capacity, and the host's ceiling covers the
    /// job's network level and each of its host ports). A host that holds a port a job declares is
    /// capable of it but has no room for it until that allocation is freed: the job waits, and
    /// closes the host to the jobs behind it as a job waiting for room does. A job that books the
    /// whole host is not walked, nor is one whose constraints are no platform requirements
    /// (logged): neither closes a host.
    pub(crate) fn walk_jobs(
        &mut self,
        queued: &[JobRecord],
        open: &BTreeMap<HostId, NetworkCeiling>,
        now: FarmTime,
    ) -> Vec<(JobId, HostId, Booked)> {
        let nodes: BTreeMap<HostId, (super::Node, &NetworkCeiling)> = open
            .iter()
            .filter_map(|(host, ceiling)| {
                let node = self.hosts.get(host).and_then(|shape| map::host(shape).ok());
                node.map(|node| (host.clone(), (node, ceiling)))
            })
            .collect();
        for (host, ((capacity, caps), _)) in &nodes {
            let heard = Event::Capacity {
                worker: map::worker(host),
                capacity: *capacity,
                caps: caps.clone(),
            };
            self.feed(now, heard);
        }
        let walkers: Vec<Walker> = queued.iter().filter_map(walker).collect();
        let usable: Vec<BTreeSet<&HostId>> = walkers
            .iter()
            .map(|walker| {
                let fits =
                    |(host, ((_, caps), ceiling)): &(&HostId, &(super::Node, &NetworkCeiling))| {
                        let capacity = self.sched.capacity(&map::worker(host));
                        let covered = within_ceiling(ceiling, walker.network, &walker.host_ports);
                        capacity.is_some_and(|capacity| {
                            walker.request.runnable_on(caps, &capacity) & covered.is_ok()
                        })
                    };
                nodes.iter().filter(fits).map(|(host, _)| host).collect()
            })
            .collect();
        let hosts: BTreeSet<HostId> = nodes.keys().cloned().collect();
        let order: Vec<usize> = (0..walkers.len()).collect();
        let placed = js_sched::walk_in_order(
            &order,
            &hosts,
            |at, host| usable[*at].contains(host),
            |at, room| self.try_job(&walkers[*at], (room, &usable[*at]), now),
        );
        let decided = walkers.into_iter().zip(placed);
        decided
            .filter_map(|(walker, host)| host.map(|host| (walker.job, host, walker.booked)))
            .collect()
    }

    /// One placement round for `walker`, held to `room`, the hosts the walk leaves open to
    /// it, those of them in `capable` (the hosts it can run on, with a ceiling that covers it)
    /// where no allocation the engine holds has a host port it declares: the host it was
    /// placed on, held in the engine, or `None`, its operation forgotten.
    fn try_job(
        &mut self,
        walker: &Walker,
        (room, capable): (&BTreeSet<HostId>, &BTreeSet<&HostId>),
        now: FarmTime,
    ) -> Option<HostId> {
        let operation = self.sched.next_operation();
        self.sched
            .recover(JOB_TERM, operation, operation.0, Vec::new());
        let free = room.iter().filter(|host| capable.contains(host));
        let free = free.filter(|host| !self.port_held_on(host, &walker.host_ports));
        let only = free.map(map::worker).collect();
        let request = (operation, walker.request.clone());
        let granted = self.place_one(request, (walker.wall, walker.charge), only, now);
        let placed = granted.and_then(|(lease, worker)| {
            let host = room.iter().find(|host| map::worker(host) == worker);
            host.map(|host| (lease, host.clone()))
        });
        match placed {
            Some((lease, host)) => {
                let placed = Placed { lease, operation };
                let booked = (walker.booked, walker.host_ports.clone());
                self.hold_job(&walker.job, &host, booked, placed);
                Some(host)
            }
            None => {
                let _ = self.sched.forget_queued(operation);
                None
            }
        }
    }
}

/// The queued `job` as a pass offers it to the engine; `None` for a job that books the whole
/// host, and for one whose constraints are no platform requirements (logged).
fn walker(job: &JobRecord) -> Option<Walker> {
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
    })
}

/// The scheduler's request for the queued `job`, which needs `resources`: its capabilities,
/// with its `os` and `arch` when it names them, as platform requirements, its GPU class as
/// `gpu.kind`, and the booking of `resources`, as an `action` lease at `batch` urgency,
/// named by its job id. How it runs (`exec`) is the host's business, not placement's.
///
/// # Errors
/// As [`map::needs_of`].
fn job_request(job: &JobRecord, resources: &Resources) -> Result<Request, MapError> {
    let constraints = &job.spec.constraints;
    let mut capabilities = constraints.capabilities.clone();
    let named = [("os", &constraints.os), ("arch", &constraints.arch)];
    let named = named.into_iter().filter_map(|(key, value)| {
        let value = value.clone();
        value.map(|value| (key.to_owned(), value))
    });
    capabilities.extend(named);
    Ok(Request {
        key: WorkKey::new("job", job.id.as_str()),
        qos: Qos::Batch,
        kind: LeaseKind::Action,
        resources: map::booking(resources),
        needs: map::needs_of(&capabilities, resources)?,
        hermetic: false,
        do_not_cache: true,
        whole_worker: false,
        pin: None,
    })
}

/// The scheduler's booking of `booked`, and whether it is the whole host.
fn booking(booked: Booked) -> (js_types::sched::Resources, bool) {
    match booked {
        Booked::Shape {
            cpu_millis,
            memory_bytes,
            disk_bytes,
            accelerators,
        } => {
            let resources = js_types::sched::Resources::new(cpu_millis, memory_bytes)
                .with_gpus(accelerators)
                .with_disk_bytes(disk_bytes);
            (resources, false)
        }
        Booked::WholeHost => (js_types::sched::Resources::new(0, 0), true),
    }
}

/// Where the start of each committed placement goes: a host is told of a placement only
/// through this, and only after the store reported the commit.
pub(crate) trait JobStarts {
    /// The placement of `job` on `host` is committed.
    fn placed(&self, job: &JobId, host: &HostId);
}

/// The server's [`JobStarts`]: it hands nothing on, since a host's start is built from the
/// job table on its next heartbeat ([`crate::per_job`]).
pub(crate) struct NoStarts;

impl JobStarts for NoStarts {
    fn placed(&self, _: &JobId, _: &HostId) {}
}

impl SchedPlacer {
    /// One pass over the queued jobs of `jobs`, on `open`, the hosts that may take a job now, each
    /// with the network ceiling it advertises, handing each committed placement to `starts` (the
    /// steps of the module docs): the jobs it placed, in queue order. A placement whose commit
    /// fails is logged and let go; its job stays queued for the next pass.
    ///
    /// # Errors
    /// The [`JobStoreError`] of a failed read of the tables: nothing is placed.
    pub(crate) fn place_jobs(
        &self,
        jobs: &ServerJobs,
        open: &BTreeMap<HostId, NetworkCeiling>,
        starts: &dyn JobStarts,
    ) -> Result<Vec<JobId>, JobStoreError> {
        let _pass = self.pass();
        let live = jobs.live_allocations()?;
        let queued = jobs.queued()?;
        let now = self.now();
        let decided = {
            let mut engine = self.engine();
            engine.sync_jobs(&live);
            engine.walk_jobs(&queued, open, now)
        };
        let mut placed = Vec::new();
        for (job, host, booked) in decided {
            match jobs.place(&job, host.clone(), booked) {
                Ok(_) => {
                    starts.placed(&job, &host);
                    placed.push(job);
                }
                Err(error) => {
                    self.engine().job_failed(&job);
                    warn!(%job, %host, %error, "a queued job's placement was not committed; it stays queued");
                }
            }
        }
        Ok(placed)
    }
}

/// Whether the host `view` may take a job at `now`: the registry hears it, it serves, it is
/// not tainted, not set aside (for a whole-node request, or to be scaled away), not still
/// clearing its last lease and not cordoned.
pub(crate) fn open_to_jobs(view: &HostView, now: Timestamp) -> bool {
    let checks = [
        view.state == HostState::Live,
        view.phase == Phase::Serving,
        view.taint.is_none(),
        view.mark.is_none(),
        !view.releasing,
        view.cordon_until <= Some(now),
    ];
    checks.into_iter().all(|ok| ok)
}

impl Agents {
    /// One pass over the queued jobs ([`SchedPlacer::place_jobs`]) on the hosts the registry
    /// would let take a job now, when this server holds the job tables and places with
    /// `js-sched` (`--placement js-sched`); otherwise nothing, so no job is placed under the
    /// registry's placement. A failed read of the tables is logged; the next pass reads them
    /// again.
    pub(crate) fn place_jobs(&self) {
        let (Some(jobs), Some(placer)) = (&self.jobs, &self.placer) else {
            return;
        };
        let now = self.clock.now();
        let hosts = self.registry.hosts().into_iter();
        let open = hosts.filter(|view| open_to_jobs(view, now)).map(|view| {
            // The ceiling is read again on every pass, from the hello the host last said; a
            // host whose hello is not held (after a restart) advertises none.
            let claim = view.claim.as_ref();
            let ceiling = claim.map(|claim| ceiling_of(claim.capabilities()));
            (view.id, ceiling.unwrap_or_default())
        });
        if let Err(error) = placer.place_jobs(jobs, &open.collect(), &NoStarts) {
            warn!(%error, "the queued jobs could not be read; the next pass places them");
        }
    }
}

#[cfg(test)]
impl SchedPlacer {
    /// What the engine's scheduler books on `host`: its leases and job allocations.
    pub(crate) fn booked_on(&self, host: &str) -> Option<js_types::sched::Resources> {
        let worker = js_types::sched::WorkerId::new(host);
        self.engine().sched.booked(&worker)
    }

    /// How many operations wait in the engine's scheduler: a job a pass did not place leaves
    /// none behind, so none holds or marks a host.
    pub(crate) fn waiting(&self) -> usize {
        self.engine().sched.queued_len()
    }
}

#[cfg(test)]
#[path = "jobs_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "jobs_ports_tests.rs"]
mod ports_tests;
