//! The queued jobs placed by the engine, and the job allocations it books (design sections
//! 5.3, steps 5 and 6, 5.4, 6.2 and 6.7, and section 8, rows 11, 12, 17 and 25).
//!
//! With the job tables, the server runs a **pass** over the
//! queue on each tick and after each heartbeat of a host that speaks the per-job messages
//! (the server's `Agents::place_jobs`): a job end, a cancel, a confirmed container, a host that
//! joins and a heartbeat that changes a host's load all come to the server as one of those. A
//! pass ([`SchedPlacer::place_jobs_with`]):
//!
//! 1. **Reads the tables as they stand**: every live (booked or releasing) allocation, every
//!    queued job in queue order and every mark. The engine holds no list of its own that a job
//!    must stay on, so no timer can drop a queued job and none is ever presented again (row
//!    12).
//! 2. **Brings the engine's bookings to the allocation table** ([`Engine::sync_jobs`]): the
//!    allocation table is the one record of what a job books on a host: its resources and
//!    its host ports. A booking freed (by the host's confirmation or the provider's absence,
//!    the server's `job_alloc`) is let go, with its ports; a booking the engine does not hold
//!    (after a restart) is taken up before anything is placed, with the ports the table
//!    records for it, so it is never offered again and its job is never placed twice. Booked
//!    and releasing allocations both book, and both hold their ports until freed.
//! 3. **Queues the jobs that need the whole host** ([`Engine::whole_jobs`], design section
//!    5.4) and writes what the engine decides for them, step by step in its order: a mark
//!    made or changed ([`js_jobs::JobStore::put_mark`]), a mark deleted
//!    ([`js_jobs::JobStore::delete_mark`]), and a job committed on the quiet host it was
//!    granted ([`js_jobs::JobStore::place_with`]: the job placed, the whole host booked, the
//!    mark deleted and the prepare deadline set in one transaction). Only after the store
//!    reports a commit does the engine hold the allocation ([`Engine::whole_job_placed`]); a
//!    commit that failed puts the grant back and the job stays queued
//!    ([`Engine::whole_job_failed`]). A host that holds such a job's mark takes no other job,
//!    except as backfill, which the walk below decides.
//! 4. **Walks the queue** ([`Engine::walk_jobs`]) in the binding order of
//!    [`js_sched::walk_in_order`], one placement round of the scheduler per job, over the
//!    hosts the registry would let take a job now ([`open_to_jobs`]), each as an [`OpenHost`]:
//!    the network ceiling its hello advertises and whether it reported that it runs host jobs,
//!    read again on every pass. A host is capable of a job only if it can run it and its
//!    ceiling covers the job's network level and each host port the job declares
//!    ([`js_sched::ceiling::within_ceiling`]); a host that advertised nothing reads as level
//!    none and no port. A job with host isolation (it runs on the machine itself) is capable
//!    only on a host that reported it runs host jobs, and a host such a job is placed on is
//!    open to no job behind it in the same walk, since the host is tainted from the job's
//!    commit on. The engine's free capacity on a host is what its
//!    shape leaves after the job allocations and the registry's lease bookings on it, which
//!    the engine holds side by side, so a job and a lease can never book the same room (row
//!    25). A host port is a resource of the allocation too: a host where an allocation the
//!    engine holds already has a port the job declares has no room for it, so one host never
//!    holds a port twice, and the job waits as for room. Each placement is held in the
//!    engine at once, with its ports, so a job or a lease placed while the pass commits does
//!    not take its room or its port. A job placed on a host a whole-host job marks (backfill)
//!    gets the wall limit of the time left in the mark's tail less the backfill margin, when
//!    that is shorter than its own ([`super::whole_jobs::backfill_wall`]).
//! 5. **Commits each placement** with [`js_jobs::JobStore::place_with`]: in one transaction the
//!    job becomes placed on the host and its allocation is written as booked, with the host
//!    ports the job declares and the wall limit of step 4. Only after the store reports the
//!    commit is the host's start handed on ([`JobStarts::placed`]), and for a host-isolation
//!    job the host's taint after it ([`JobStarts::tainted`]); the taint itself is a row of the
//!    same transaction (written by the job store). A failed commit lets the engine's hold go
//!    ([`Engine::job_failed`]): nothing is booked, the job stays queued and the next pass
//!    places it. A job that books the whole host and runs on the host itself is not marked or
//!    committed by the whole-host pass: it stays queued.
//! 6. **Records why each job waits** (`docs/design/queue-status.md`): what pass 2 told of a
//!    job since the last pass ([`SchedPlacer::note_outcomes`]), then the pass's own outcome
//!    for it, decided by [`js_sched::queue_status::notes`] against the code the job showed.
//!    A placement's notes ride in its commit ([`Placement::queue`]); a job the walk placed
//!    nowhere waits for capacity, and a whole-host job that was not committed waits for a
//!    whole host, naming the host its mark holds. A job whose status the notes would not
//!    change is not written ([`js_jobs::QueueTrail::changed_by`]); the others are written
//!    with [`js_jobs::JobStore::note_queue`], and one that fails is logged and counted and
//!    written by a later pass: what pass 2 told of a job whose placement or status write did
//!    not land is given back, for the job's status write after it in the pass or for the next
//!    pass, before what pass 2 told since. The pass is the one writer of a job's queue status: pass 2
//!    writes none.
//!
//! The start rides on the host's next heartbeat reply, built from the job table
//! (the server's `per_job`): so the server's [`JobStarts`] hands no start on (it only collects
//! the taints told, see [`NoStarts`] for one that hands nothing on), and a placement
//! whose start was never delivered (the server stopped after the commit) is started from the
//! table after a restart all the same.
//!
//! A job that books the whole host is not walked: step 3 places it, and the allocation it
//! books holds the whole host in the scheduler, so no job with a port is placed beside it
//! and a request for the whole host waits for every holder. A lease is never in the queue: it
//! is placed on arrival by the registry's placer, which sees the job allocations too.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use js_agent_wire::{NetworkCeiling, Phase};
use js_jobs::{
    AllocationRecord, Booked, IdSource, Isolation, JobId, JobRecord, JobStore, JobStoreError,
    Needs, NetworkPolicy, Placement,
};
use js_registry::{HostState, HostView, WholeNodeConfig};
use js_sched::ceiling::within_ceiling;
use js_sched::queue_status::{self, Outcome};
use js_sched::{Event, Prediction, Request};
use js_types::Timestamp;
use js_types::sched::{FarmTime, LeaseId as SchedLeaseId, LeaseKind, Qos, WorkKey};
use js_types::{HostId, JobNetwork, QueueCode, QueueNote, Resources};
use tracing::warn;

use super::whole_jobs::{WholeJobStep, WholeJobs, backfill_wall};
use super::{Engine, Placed, recovered_as};
use crate::map::{self, MapError};
use crate::place::SchedPlacer;

/// The network level a job's policy asks for.
pub fn job_network(policy: NetworkPolicy) -> JobNetwork {
    match policy {
        NetworkPolicy::None => JobNetwork::None,
        NetworkPolicy::AllowList => JobNetwork::AllowList,
        NetworkPolicy::Open => JobNetwork::Open,
    }
}

/// What a pass tells the server's metrics: each committed placement, with how long its job
/// waited since its submit, and each write of the pass whose commit failed.
pub trait PassMetrics {
    /// A queued job was placed, `waited` after it was submitted.
    fn job_placed(&self, waited: Duration);
    /// A write of the pass failed (a placement's commit, or a whole-host job's mark): it did
    /// not land.
    fn store_write_failed(&self);
}

/// Whether `error` says the state store has a newer writer, so this server commits nothing
/// more.
fn is_fenced(error: &JobStoreError) -> bool {
    matches!(error, JobStoreError::Store(js_state::StoreError::Fenced))
}

/// The term of the leases the engine places for jobs: each is numbered by its operation, so
/// none is a lease of the registry's (term 1) or one taken up (term 0).
pub const JOB_TERM: u64 = 2;

/// A placement the walk decided: the job, its host, what it books there and what its commit
/// writes beside the booking ([`Engine::walk_jobs`]).
pub type Decided = (JobId, HostId, Booked, Placement);

/// A job allocation the engine holds.
#[derive(Debug, Clone)]
pub struct JobHold {
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

/// A host a pass may place on: the network ceiling its hello advertises and whether it reported
/// that it runs host jobs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OpenHost {
    /// The most network the host's agent gives a job.
    pub ceiling: NetworkCeiling,
    /// Whether the host's hello reported `host-jobs`.
    pub host_jobs: bool,
}

/// A queued job, as a pass offers it to the engine.
struct Walker {
    job: JobId,
    /// Whether the job runs on the machine itself (host isolation).
    host_isolation: bool,
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
    pub fn holds_jobs_on(&self, host: &HostId) -> bool {
        self.jobs.values().any(|hold| hold.host == *host)
    }

    /// The engine's leases of the job allocations it holds on `host`.
    pub fn job_leases_on<'a>(
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
    pub(super) fn hold_job(
        &mut self,
        job: &JobId,
        host: &HostId,
        booked: (Booked, Vec<u16>),
        placed: Placed,
    ) {
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
    pub fn sync_jobs(&mut self, live: &[AllocationRecord]) {
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
    pub fn job_failed(&mut self, job: &JobId) {
        let placed = self.jobs.remove(job).map(|hold| hold.placed);
        placed.into_iter().for_each(|placed| self.release(placed));
    }

    /// One walk of `queued`, the queued jobs in queue order, over `open`, the hosts that may take a
    /// job now with the network ceiling each advertises and whether it runs host jobs, at `now`:
    /// the placements it decides, each as the job, its host, what it books and what else the
    /// commit writes (the wall limit of a job placed on a host a whole-host job marks), in queue
    /// order. Each is held in the engine from here on, so it books its host for whatever is
    /// placed after it, until its commit answers.
    ///
    /// Each open host the engine knows by a shape it can describe is heard from now, with its
    /// capacity, as at a lease's start; the others are left out. The walk is
    /// [`js_sched::walk_in_order`]: each job is placed by one placement round held to the hosts the
    /// walk leaves open to it (the same load placement, CPU slack and memory percent as a lease),
    /// and a job placed nowhere closes to the jobs behind it every host it could run on with room
    /// (its capabilities match and it fits the host's capacity, and the host's ceiling covers the
    /// job's network level and each of its host ports). A host that holds a port a job declares is
    /// capable of it but has no room for it until that allocation is freed: the job waits, and
    /// closes the host to the jobs behind it as a job waiting for room does. A job with host
    /// isolation is capable only of a host that runs host jobs, and the host it is placed on is
    /// left out of the room of every job behind it in the walk. A job that books the
    /// whole host is not walked, nor is one whose constraints are no platform requirements
    /// (logged): neither closes a host.
    ///
    /// Returns the placements, and each walked job placed nowhere with whether some open host
    /// could run it (it waits for room) or none could, both in queue order.
    pub fn walk_jobs(
        &mut self,
        queued: &[JobRecord],
        open: &BTreeMap<HostId, OpenHost>,
        now: FarmTime,
    ) -> (Vec<Decided>, Vec<(JobId, bool)>) {
        let nodes: BTreeMap<HostId, (super::Node, &OpenHost)> = open
            .iter()
            .filter_map(|(host, offer)| {
                let node = self.hosts.get(host).and_then(|shape| map::host(shape).ok());
                node.map(|node| (host.clone(), (node, offer)))
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
                let fits = |(host, ((_, caps), offer)): &(&HostId, &(super::Node, &OpenHost))| {
                    let capacity = self.sched.capacity(&map::worker(host));
                    let ports = &walker.host_ports;
                    let covered = within_ceiling(&offer.ceiling, walker.network, ports);
                    let allowed = !walker.host_isolation | offer.host_jobs;
                    capacity.is_some_and(|capacity| {
                        walker.request.runnable_on(caps, &capacity) & covered.is_ok() & allowed
                    })
                };
                nodes.iter().filter(fits).map(|(host, _)| host).collect()
            })
            .collect();
        let hosts: BTreeSet<HostId> = nodes.keys().cloned().collect();
        let order: Vec<usize> = (0..walkers.len()).collect();
        // The hosts a host-isolation job was placed on in this walk: tainted from its commit on,
        // so open to no job behind it.
        let mut tainted: BTreeSet<HostId> = BTreeSet::new();
        let placed = js_sched::walk_in_order(
            &order,
            &hosts,
            |at, host| usable[*at].contains(host),
            |at, room| {
                let walker = &walkers[*at];
                let room: BTreeSet<HostId> = room.difference(&tainted).cloned().collect();
                let host = self.try_job(walker, (&room, &usable[*at]), now);
                tainted.extend(host.clone().filter(|_| walker.host_isolation));
                host
            },
        );
        let decided = walkers.into_iter().zip(placed).zip(&usable);
        let mut placements = Vec::new();
        let mut waiting = Vec::new();
        for ((walker, host), capable) in decided {
            match host {
                Some(host) => {
                    let placement = self.backfill_placement(walker.wall, &host, now);
                    placements.push((walker.job, host, walker.booked, placement));
                }
                None => waiting.push((walker.job, !capable.is_empty())),
            }
        }
        (placements, waiting)
    }

    /// What the commit of a job placed on `host` writes beside its booking: when a whole-host job
    /// marks `host` and the mark's tail ends, the wall limit is cut to the time left in the tail
    /// less the backfill margin ([`backfill_wall`]), if that is shorter than the job's own
    /// `wall`; otherwise nothing.
    fn backfill_placement(&self, wall: Duration, host: &HostId, now: FarmTime) -> Placement {
        let marked = self.sched.whole_room(&map::worker(host)).marked_for;
        let mark = marked.and_then(|operation| self.sched.whole_mark(operation));
        let tail = mark.and_then(|mark| mark.tail_end);
        let cut = tail.map(|end| backfill_wall(wall, end, now));
        let cut = cut.filter(|cut| *cut < wall);
        Placement {
            wall_limit_secs: cut.map(|cut| cut.as_secs()),
            ..Placement::default()
        }
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
        // The whole-node requests are placed first, numbered in their own term.
        self.whole_tick(now);
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
/// host (the pass queues it with [`Engine::whole_jobs`]), and for one whose constraints are no
/// platform requirements (logged).
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
        host_isolation: job.spec.constraints.isolation == Isolation::Host,
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
pub fn job_request(job: &JobRecord, resources: &Resources) -> Result<Request, MapError> {
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
pub trait JobStarts {
    /// The placement of `job` on `host` is committed.
    fn placed(&self, job: &JobId, host: &HostId);
    /// The placement of `job` on `host`, called after [`JobStarts::placed`], tainted the host:
    /// the job runs with host isolation, and its commit also wrote the taint. Not called for a
    /// contained job.
    fn tainted(&self, host: &HostId, job: &JobId);
}

/// A [`JobStarts`] that hands nothing on and keeps no taint: a host's start is built from the
/// job table on its next heartbeat (the server's `per_job`), and the server's own
/// [`JobStarts`] only collects the taints told.
pub struct NoStarts;

impl JobStarts for NoStarts {
    fn placed(&self, _: &JobId, _: &HostId) {}

    fn tainted(&self, _: &HostId, _: &JobId) {}
}

/// The hosts a pass places on, as the server reads them from the registry.
#[derive(Debug, Clone, Copy)]
pub struct JobHosts<'a> {
    /// The hosts that may take a job now, each with the network ceiling it advertises and
    /// whether it runs host jobs.
    pub open: &'a BTreeMap<HostId, OpenHost>,
    /// The hosts that said hello to this server.
    pub hello: &'a BTreeSet<HostId>,
    /// The pool's whole-node settings, if it has any.
    pub whole: Option<&'a WholeNodeConfig>,
}

impl SchedPlacer {
    /// One pass over the queued jobs of `jobs`, on `open`, the hosts that may take a job now,
    /// each with the network ceiling it advertises and whether it runs host jobs:
    /// [`SchedPlacer::place_jobs_with`] for a pool with no whole-node settings and no host that
    /// said hello, so no job that needs the whole host is placed.
    ///
    /// # Errors
    /// As [`SchedPlacer::place_jobs_with`].
    pub fn place_jobs<S: js_state::Store, I: IdSource>(
        &self,
        jobs: &JobStore<S, I>,
        open: &BTreeMap<HostId, OpenHost>,
        (starts, metrics): (&dyn JobStarts, &dyn PassMetrics),
    ) -> Result<Vec<JobId>, JobStoreError> {
        let hello = BTreeSet::new();
        let hosts = JobHosts {
            open,
            hello: &hello,
            whole: None,
        };
        self.place_jobs_with(jobs, &hosts, (starts, metrics))
    }

    /// One pass over the queued jobs of `jobs`, on `hosts`, handing each committed placement to
    /// `starts` (the steps of the module docs): the jobs it placed, in queue order, those that
    /// need the whole host first. A write that fails is logged and let go, and a placement whose
    /// commit fails leaves its job queued for the next pass. Each committed placement, with how
    /// long its job waited since its submit, and each write that did not commit, are told to
    /// `metrics`.
    ///
    /// The marks of the whole-host jobs wait for their hosts to say hello until the server's
    /// start plus the pool's `regain_grace` (design section 6.7, step 3). A job that needs the
    /// whole host is committed with the pool's `prepare_timeout` as its allocation's prepare
    /// deadline.
    ///
    /// # Errors
    /// The [`JobStoreError`] of a failed read of the tables: nothing is placed. A write the
    /// store answers with [`js_state::StoreError::Fenced`] is answered too, and the pass ends
    /// there: a store with a newer writer commits nothing more.
    pub fn place_jobs_with<S: js_state::Store, I: IdSource>(
        &self,
        jobs: &JobStore<S, I>,
        hosts: &JobHosts<'_>,
        (starts, metrics): (&dyn JobStarts, &dyn PassMetrics),
    ) -> Result<Vec<JobId>, JobStoreError> {
        let _pass = self.pass();
        let live = jobs.live_allocations()?;
        let queued = jobs.queued()?;
        let marks = jobs.marks()?;
        let now = self.now();
        let grace = hosts.whole.map_or(0, |whole| whole.regain_grace_ms);
        let open: BTreeSet<HostId> = hosts.open.keys().cloned().collect();
        // The whole-host engine marks and commits only the jobs that book the whole host and
        // run contained: it does not yet hold a job to the hosts that run host jobs, so a
        // whole-host job with host isolation stays queued.
        let whole: Vec<JobRecord> = queued
            .iter()
            .filter(|job| {
                let needs_whole = matches!(job.spec.needs, Needs::WholeHost);
                needs_whole & (job.spec.constraints.isolation != Isolation::Host)
            })
            .cloned()
            .collect();
        let input = WholeJobs {
            queued: &whole,
            marks: &marks,
            hello: hosts.hello,
            open: &open,
            settings: hosts.whole,
            claim_by: self.boot().saturating_add(Duration::from_millis(grace)),
        };
        let steps = {
            let mut engine = self.engine();
            engine.sync_jobs(&live);
            engine.whole_jobs(&input, now)
        };
        let mut noted = Noted {
            codes: queued
                .iter()
                .map(|job| (job.id.clone(), job.queue.code()))
                .collect(),
            outcomes: self.take_outcomes(),
            taken: BTreeMap::new(),
        };
        let mut emptying: BTreeMap<JobId, HostId> = marks
            .iter()
            .map(|mark| (mark.job.clone(), mark.host.clone()))
            .collect();
        let mut placed = Vec::new();
        for step in steps {
            match &step {
                WholeJobStep::Mark(mark) => {
                    drop(emptying.insert(mark.job.clone(), mark.host.clone()))
                }
                WholeJobStep::Unmark(job) => drop(emptying.remove(job)),
                WholeJobStep::Commit { .. } => {}
            }
            match self.write_step(jobs, &step, hosts.whole, (starts, metrics), &mut noted) {
                Ok(job) => placed.extend(job),
                Err(error) => {
                    warn!(?step, %error, "a write for a whole-host job was not committed; the job stays queued");
                    failed_write(error, metrics)?;
                }
            }
        }
        let (decided, waiting) = self.engine().walk_jobs(&queued, hosts.open, now);
        for (job, host, booked, mut placement) in decided {
            placement.queue = noted.placed(&job, &host);
            match jobs.place_with(&job, host.clone(), booked, placement) {
                Ok((record, _)) => {
                    told(&record, &host, (starts, metrics));
                    placed.push(job);
                }
                Err(error) => {
                    self.engine().job_failed(&job);
                    noted.unwritten(&job);
                    warn!(%job, %host, %error, "a queued job's placement was not committed; it stays queued");
                    // A store with a newer writer commits nothing more: the pass ends here
                    // and answers the error, for the caller to trip the fence on.
                    failed_write(error, metrics)?;
                }
            }
        }
        let mut waits: BTreeMap<JobId, Outcome> = waiting
            .into_iter()
            .map(|(job, capable)| (job, Outcome::Waiting { capable }))
            .collect();
        for job in &whole {
            let host = emptying.get(&job.id).cloned();
            waits.insert(job.id.clone(), Outcome::WholeHost { emptying: host });
        }
        let committed: BTreeSet<&JobId> = placed.iter().collect();
        let unplaced = queued.iter().filter(|job| !committed.contains(&job.id));
        for job in unplaced {
            let notes = noted.waiting(&job.id, waits.remove(&job.id));
            if !job.queue.changed_by(&notes) {
                continue;
            }
            if let Err(error) = jobs.note_queue(&job.id, &notes) {
                warn!(job = %job.id, %error, "a queued job's queue status was not written; the next pass writes it");
                noted.unwritten(&job.id);
                self.restore_outcomes(noted.outcomes.remove_entry(&job.id).into_iter().collect());
                failed_write(error, metrics)?;
            }
        }
        Ok(placed)
    }

    /// Writes `step`, one decision of the engine for a whole-host job: the job a commit placed,
    /// after the engine holds its allocation and `metrics` and `starts` are told. A commit that
    /// fails puts the job's grant back in the engine.
    fn write_step<S: js_state::Store, I: IdSource>(
        &self,
        jobs: &JobStore<S, I>,
        step: &WholeJobStep,
        settings: Option<&WholeNodeConfig>,
        (starts, metrics): (&dyn JobStarts, &dyn PassMetrics),
        noted: &mut Noted,
    ) -> Result<Option<JobId>, JobStoreError> {
        match step {
            WholeJobStep::Mark(mark) => {
                jobs.put_mark(mark.clone())?;
                Ok(None)
            }
            WholeJobStep::Unmark(job) => {
                jobs.delete_mark(job)?;
                Ok(None)
            }
            WholeJobStep::Commit { job, host } => {
                let prepare_timeout =
                    settings.map(|whole| Duration::from_millis(whole.prepare_timeout_ms));
                let placement = Placement {
                    prepare_timeout,
                    wall_limit_secs: None,
                    queue: noted.placed(job, host),
                };
                let placed = jobs.place_with(job, host.clone(), Booked::WholeHost, placement);
                let (record, allocation) = match placed {
                    Ok(placed) => placed,
                    Err(error) => {
                        self.engine().whole_job_failed(job);
                        noted.unwritten(job);
                        return Err(error);
                    }
                };
                let mut engine = self.engine();
                engine.whole_job_placed(job, host, allocation.host_ports);
                drop(engine);
                told(&record, host, (starts, metrics));
                Ok(Some(job.clone()))
            }
        }
    }
}

/// What a pass records on each queued job's queue status (`docs/design/queue-status.md`): the
/// code each showed when the pass read it, and what pass 2 told of each since the last pass.
pub(super) struct Noted {
    codes: BTreeMap<JobId, QueueCode>,
    outcomes: BTreeMap<JobId, Vec<Outcome>>,
    /// What pass 2 told of each job whose notes were taken this pass, until its write lands.
    taken: BTreeMap<JobId, Vec<Outcome>>,
}

impl Noted {
    /// The notes of `job`: what pass 2 told of it, then `last`, the pass's own outcome.
    fn notes(&mut self, job: &JobId, last: Option<Outcome>) -> Vec<QueueNote> {
        let current = self.codes.get(job).copied().unwrap_or_default();
        let told = self.outcomes.remove(job).unwrap_or_default();
        let mut outcomes = told.clone();
        self.taken.insert(job.clone(), told);
        outcomes.extend(last);
        queue_status::notes(current, &outcomes)
    }

    /// What pass 2 told of `job`, whose write with its notes did not land, given back: to this
    /// pass, for the job's queue status write after a placement that failed.
    fn unwritten(&mut self, job: &JobId) {
        let told = self.taken.remove(job).unwrap_or_default();
        self.outcomes.insert(job.clone(), told);
    }

    /// The notes of `job`, placed on `host`, for its placement's commit.
    fn placed(&mut self, job: &JobId, host: &HostId) -> Vec<QueueNote> {
        let host = host.clone();
        self.notes(job, Some(Outcome::Placed { host }))
    }

    /// The notes of the queued `job` the pass did not place, which waits as `wait` says (none
    /// for a job the pass does not walk).
    fn waiting(&mut self, job: &JobId, wait: Option<Outcome>) -> Vec<QueueNote> {
        self.notes(job, wait)
    }
}

/// Tells `metrics` and `starts` of the placement of the job `record` shows on `host`, which the
/// store committed: its start, and for a job that runs on the host itself the host's taint.
fn told(record: &JobRecord, host: &HostId, (starts, metrics): (&dyn JobStarts, &dyn PassMetrics)) {
    let at = record.placed_at.unwrap_or(record.submitted_at);
    metrics.job_placed(at.since(record.submitted_at));
    starts.placed(&record.id, host);
    if record.spec.constraints.isolation == Isolation::Host {
        starts.tainted(host, &record.id);
    }
}

/// Tells `metrics` that a write of the pass failed with `error`. The error again when the store
/// has a newer writer, for the pass to end with.
fn failed_write(error: JobStoreError, metrics: &dyn PassMetrics) -> Result<(), JobStoreError> {
    metrics.store_write_failed();
    if is_fenced(&error) {
        Err(error)
    } else {
        Ok(())
    }
}

/// Whether the host `view` may take a job at `now`: the registry hears it, it serves, it is
/// not tainted, not set aside (for a whole-node request, or to be scaled away), not still
/// clearing its last lease and not cordoned.
pub fn open_to_jobs(view: &HostView, now: Timestamp) -> bool {
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

#[cfg(any(test, feature = "test-support"))]
impl SchedPlacer {
    /// What the engine's scheduler books on `host`: its leases and job allocations.
    pub fn booked_on(&self, host: &str) -> Option<js_types::sched::Resources> {
        let worker = js_types::sched::WorkerId::new(host);
        self.engine().sched.booked(&worker)
    }

    /// What pass 2 told of each job that the next pass has still to take
    /// ([`SchedPlacer::note_outcomes`]), left as it is.
    pub fn outcomes_noted(&self) -> BTreeMap<JobId, Vec<Outcome>> {
        let taken = self.take_outcomes();
        self.restore_outcomes(taken.clone());
        taken
    }

    /// How many operations wait in the engine's scheduler: a job a pass did not place leaves
    /// none behind, so none holds or marks a host.
    pub fn waiting(&self) -> usize {
        self.engine().sched.queued_len()
    }
}

#[cfg(test)]
#[path = "jobs_tests.rs"]
mod tests;
