//! The host ports and the network level of the queued jobs a pass places: a port is booked
//! with the allocation and held by one job per host until it is freed, a host whose
//! advertised ceiling is below what a job asks is not capable of it, and a job that asks for
//! neither is placed as before. Every clock is manual; nothing sleeps.

use std::sync::{Arc, Mutex};

use js_agent_wire::PortRange;
use js_jobs::{DeclaredPort, JobSpec, JobState, NetworkPolicy};
use js_lease::ManualClock;
use js_registry::Credential;
use js_sched::ceiling::BeyondCeiling;

use super::tests::{
    NONE, booked, clock, finish, host, pass, placer_on, queue, queue_spec, run_to_exit, sized,
    state,
};
use super::*;
use crate::agent_api::job_rig::{placed, spec, tables};
use crate::agents::assemble_placed;
use crate::job_alloc::{Freed, answer_retired, free_allocation};
use crate::sched::engine_rig::{GIB, hello_of, pool};
use crate::sched::wiring_tests::{JOIN, config_of, scratch};

/// The ceiling of a host that gives `level` and forwards each of `ports`.
fn ceiling(level: JobNetwork, ports: &[u16]) -> NetworkCeiling {
    let ports = ports.iter().map(|port| PortRange::new(*port, *port));
    NetworkCeiling {
        level,
        ports: ports.map(Option::unwrap).collect(),
    }
}

/// The hosts `names`, each advertising `ceiling`.
fn open_with(names: &[&str], ceiling: &NetworkCeiling) -> BTreeMap<HostId, NetworkCeiling> {
    let hosts = names.iter().map(|name| host(name));
    hosts.map(|host| (host, ceiling.clone())).collect()
}

/// The hosts `names`, each at the level allow-list and forwarding the host port `port`.
fn forwarding(names: &[&str], port: u16) -> BTreeMap<HostId, NetworkCeiling> {
    open_with(names, &ceiling(JobNetwork::AllowList, &[port]))
}

/// One pass of `placer` over `jobs` on `open`: the jobs it placed.
fn pass_with(
    placer: &SchedPlacer,
    jobs: &ServerJobs,
    open: &BTreeMap<HostId, NetworkCeiling>,
) -> Vec<String> {
    let placed = placer.place_jobs(jobs, open, &NoStarts).unwrap();
    placed.iter().map(ToString::to_string).collect()
}

/// A job of one core and 2 GiB at the level allow-list declaring each of the host `ports`.
fn ported(ports: &[u16]) -> JobSpec {
    let declared = ports.iter().map(|port| DeclaredPort {
        host: *port,
        container: 80,
    });
    JobSpec {
        network: NetworkPolicy::AllowList,
        ports: declared.collect(),
        ..sized(2)
    }
}

/// The host the allocation of the job `id` is on.
fn on(jobs: &ServerJobs, id: &str) -> String {
    let id = JobId::parse(id).unwrap();
    jobs.get_allocation(&id).unwrap().unwrap().host.to_string()
}

/// The host ports the allocation of the job `id` holds.
fn ports_of(jobs: &ServerJobs, id: &str) -> Vec<u16> {
    let id = JobId::parse(id).unwrap();
    jobs.get_allocation(&id).unwrap().unwrap().host_ports
}

/// Asserts that two jobs declaring the same host port are placed by one pass on different
/// hosts, though both hosts forward it and have room for both, and that each allocation
/// reads back holding the port. One host already carries more load than the other, so the
/// load alone would send the second job wherever the first went (to the emptier host if
/// placement spreads, to the fuller if it packs). It catches a pass that offers a host a port
/// a placement of the same pass just booked there.
#[test]
fn two_jobs_asking_for_the_same_host_port_land_on_different_hosts() {
    let placer = placer_on(&clock(), &["host-a", "host-b"]);
    let (_store, jobs) = tables();
    placed(&jobs, "job-load", "host-b", sized(4), booked(4));
    let open = forwarding(&["host-a", "host-b"], 8080);
    queue_spec(&jobs, "job-a", ported(&[8080]), 0);
    queue_spec(&jobs, "job-b", ported(&[8080]), 0);
    assert_eq!(pass_with(&placer, &jobs, &open), ["job-a", "job-b"]);
    assert_ne!(on(&jobs, "job-a"), on(&jobs, "job-b"));
    assert_eq!(ports_of(&jobs, "job-a"), [8080]);
    assert_eq!(ports_of(&jobs, "job-b"), [8080]);
}

/// Asserts that with one host the second job declaring its port waits in the queue with no
/// hold left in the engine, and is placed on the host by the pass after the first job's
/// allocation is freed. It catches a port that is never held, and a port held past the
/// free.
#[test]
fn the_second_job_waits_when_only_one_host_has_the_port_free() {
    let placer = placer_on(&clock(), &["host-a"]);
    let (_store, jobs) = tables();
    let open = forwarding(&["host-a"], 8080);
    let a = queue_spec(&jobs, "job-a", ported(&[8080]), 0);
    queue_spec(&jobs, "job-b", ported(&[8080]), 0);
    assert_eq!(pass_with(&placer, &jobs, &open), ["job-a"]);
    assert_eq!(state(&jobs, "job-b"), JobState::Queued);
    assert_eq!(placer.waiting(), 0);
    finish(&jobs, &a);
    assert_eq!(pass_with(&placer, &jobs, &open), ["job-b"]);
    assert_eq!(on(&jobs, "job-b"), "host-a");
}

/// Asserts that a host whose ceiling forwards other ports only is not capable of a job
/// declaring 8080: alone it leaves the job queued, the ceiling refuses the port by name, and
/// with a busier host that forwards 8080 offered too the job is placed there. It catches a
/// ceiling that is not read, and a host chosen by load alone.
#[test]
fn a_host_whose_ceiling_lacks_the_port_is_not_capable() {
    let placer = placer_on(&clock(), &["host-a", "host-b"]);
    let (_store, jobs) = tables();
    placed(&jobs, "job-load", "host-b", sized(4), booked(4));
    queue_spec(&jobs, "job-p", ported(&[8080]), 0);
    let other = ceiling(JobNetwork::AllowList, &[9000]);
    let alone = open_with(&["host-a"], &other);
    assert_eq!(pass_with(&placer, &jobs, &alone), NONE);
    assert_eq!(state(&jobs, "job-p"), JobState::Queued);
    let refused = within_ceiling(&other, JobNetwork::AllowList, &[8080]);
    assert_eq!(refused, Err(BeyondCeiling::Port(8080)));
    assert!(refused.unwrap_err().to_string().contains("8080"));

    let mut both = alone;
    both.extend(forwarding(&["host-b"], 8080));
    assert_eq!(pass_with(&placer, &jobs, &both), ["job-p"]);
    assert_eq!(on(&jobs, "job-p"), "host-b");
}

/// Asserts that a host advertising no ceiling at all (as an agent from before the keys does)
/// is never given a job that declares a port, and is given a job that asks for no network
/// and no port. It catches an absent ceiling read as no limit.
#[test]
fn a_host_that_advertised_no_ports_is_not_capable_of_a_job_with_ports() {
    let placer = placer_on(&clock(), &["host-a"]);
    let (_store, jobs) = tables();
    queue_spec(&jobs, "job-1-ports", ported(&[8080]), 0);
    queue(&jobs, "job-2-plain", 2, 0);
    assert_eq!(pass(&placer, &jobs, &["host-a"]), ["job-2-plain"]);
    assert_eq!(state(&jobs, "job-1-ports"), JobState::Queued);
    assert_eq!(pass(&placer, &jobs, &["host-a"]), NONE);
}

/// Placements of a batch of jobs that ask for no network and no port, in queue order, over
/// two hosts each advertising `ceiling`, as the job and the host.
fn portless_placements(ceiling: &NetworkCeiling) -> Vec<(String, String)> {
    let placer = placer_on(&clock(), &["host-a", "host-b"]);
    let (_store, jobs) = tables();
    for (n, gib) in [4, 2, 8, 1, 3, 6, 5].into_iter().enumerate() {
        queue(&jobs, &format!("job-{n}"), gib, 0);
    }
    let open = open_with(&["host-a", "host-b"], ceiling);
    let placed = pass_with(&placer, &jobs, &open);
    let with_host = placed.into_iter().map(|job| {
        let at = on(&jobs, &job);
        (job, at)
    });
    with_host.collect()
}

/// Asserts that a batch of jobs asking for no network and no port is placed on the same hosts
/// in the same order whether the hosts advertise no ceiling or the widest. It catches a
/// ceiling or a port check that changes where a job with neither goes.
#[test]
fn a_job_with_no_ports_is_unaffected() {
    let none = portless_placements(&NetworkCeiling::default());
    let wide = ceiling(JobNetwork::Open, &[]);
    let wide = NetworkCeiling {
        ports: vec![PortRange::new(1, 65535).unwrap()],
        ..wide
    };
    assert_eq!(none, portless_placements(&wide));
    assert!(!none.is_empty());
}

/// Asserts that a job whose end is not yet confirmed (its allocation releasing) still holds
/// its port, so the next job declaring it is not placed on that host, and that after the
/// host confirms and the allocation is freed that job is placed there. It catches a port
/// let go at the job's end instead of the allocation's free.
#[test]
fn a_freed_allocation_releases_its_ports() {
    let placer = placer_on(&clock(), &["host-a"]);
    let (_store, jobs) = tables();
    let open = forwarding(&["host-a"], 8080);
    let a = queue_spec(&jobs, "job-a", ported(&[8080]), 0);
    assert_eq!(pass_with(&placer, &jobs, &open), ["job-a"]);
    run_to_exit(&jobs, &a);
    queue_spec(&jobs, "job-b", ported(&[8080]), 0);
    assert_eq!(pass_with(&placer, &jobs, &open), NONE);
    free_allocation(&jobs, &a, Freed::HostConfirmed).unwrap();
    assert_eq!(pass_with(&placer, &jobs, &open), ["job-b"]);
}

/// Asserts that a job placed and then cancelled before its container was ever started has
/// its allocation freed by the host's answer that it holds no such job, in one commit, and
/// that the next job declaring its port is placed on the host by the next pass. It catches a
/// port kept by an allocation that was freed without a confirmation of a stop.
#[test]
fn a_cancelled_before_start_job_releases_its_ports() {
    let placer = placer_on(&clock(), &["host-a"]);
    let (_store, jobs) = tables();
    let open = forwarding(&["host-a"], 8080);
    let a = queue_spec(&jobs, "job-a", ported(&[8080]), 0);
    assert_eq!(pass_with(&placer, &jobs, &open), ["job-a"]);
    queue_spec(&jobs, "job-b", ported(&[8080]), 0);
    assert_eq!(pass_with(&placer, &jobs, &open), NONE);
    jobs.request_cancel(&a).unwrap();
    answer_retired(&jobs, &host("host-a"), &a).unwrap();
    assert_eq!(pass_with(&placer, &jobs, &open), ["job-b"]);
}

/// A [`JobStarts`] that records the host ports the job's allocation row holds when its start
/// is handed on.
struct PortsAtStart<'a> {
    jobs: &'a ServerJobs,
    seen: Mutex<Vec<Vec<u16>>>,
}

impl JobStarts for PortsAtStart<'_> {
    fn placed(&self, job: &JobId, _: &HostId) {
        let allocation = self.jobs.get_allocation(job).unwrap().unwrap();
        self.seen.lock().unwrap().push(allocation.host_ports);
    }
}

/// Asserts that the allocation row already holds the job's host ports when the start is
/// handed on, so the ports are committed with the placement; and that a placement whose
/// commit fails leaves the job queued with no allocation and holds no port, so a more urgent
/// job declaring it is placed on the host by the next pass. It catches ports booked in a
/// second commit, and a failed commit that keeps its ports.
#[test]
fn ports_commit_with_the_placement() {
    let placer = placer_on(&clock(), &["host-a"]);
    let (store, jobs) = tables();
    let open = forwarding(&["host-a"], 8080);
    let starts = PortsAtStart {
        jobs: &jobs,
        seen: Mutex::new(Vec::new()),
    };
    let a = queue_spec(&jobs, "job-a", ported(&[8080]), 0);
    placer.place_jobs(&jobs, &open, &starts).unwrap();
    assert_eq!(*starts.seen.lock().unwrap(), [[8080]]);
    finish(&jobs, &a);

    queue_spec(&jobs, "job-b", ported(&[8080]), 0);
    store.fail_next();
    assert_eq!(placer.place_jobs(&jobs, &open, &starts), Ok(Vec::new()));
    let b = JobId::parse("job-b").unwrap();
    assert_eq!(jobs.get_allocation(&b).unwrap(), None);
    assert_eq!(state(&jobs, "job-b"), JobState::Queued);
    queue_spec(&jobs, "job-c", ported(&[8080]), 5);
    assert_eq!(pass_with(&placer, &jobs, &open), ["job-c"]);
    assert_eq!(starts.seen.lock().unwrap().len(), 1);
}

/// Asserts that a placer built again over the same tables takes up the ports an allocation
/// booked: the next job declaring one is not placed on that host (it waits when it is the only
/// host open, and lands on the other host when that is offered). It catches ports kept only
/// in the memory of the engine that placed the job.
#[test]
fn booked_ports_survive_a_restart() {
    let clock = clock();
    let (_store, jobs) = tables();
    let first = placer_on(&clock, &["host-a", "host-b"]);
    let both = forwarding(&["host-a", "host-b"], 8080);
    queue_spec(&jobs, "job-a", ported(&[8080]), 0);
    assert_eq!(pass_with(&first, &jobs, &both), ["job-a"]);
    let held_on = on(&jobs, "job-a");
    drop(first);

    let second = placer_on(&clock, &["host-a", "host-b"]);
    queue_spec(&jobs, "job-b", ported(&[8080]), 0);
    let only = forwarding(&[held_on.as_str()], 8080);
    assert_eq!(pass_with(&second, &jobs, &only), NONE);
    assert_eq!(state(&jobs, "job-b"), JobState::Queued);
    assert_eq!(pass_with(&second, &jobs, &both), ["job-b"]);
    assert_ne!(on(&jobs, "job-b"), held_on);
}

/// Whether the engine of `placer` grants a request that books the whole of `host`, as a lease
/// of a whole-host class does, now; the grant is let go again at once. `template` is the
/// record of a job the request is named after.
fn whole_host_granted(placer: &SchedPlacer, template: &JobRecord, host_name: &str) -> bool {
    let mut record = template.clone();
    record.id = JobId::parse("job-whole").unwrap();
    let resources = Resources::new(1000, GIB, 0, None).unwrap();
    let mut request = job_request(&record, &resources).unwrap();
    request.whole_worker = true;
    let now = placer.now();
    let charge = Prediction {
        cpu_millis: 1000,
        memory_bytes: GIB,
        cold: false,
        life_minutes: 1,
    };
    let only = BTreeSet::from([map::worker(&host(host_name))]);
    let mut engine = placer.engine();
    let operation = engine.sched.next_operation();
    engine
        .sched
        .recover(JOB_TERM, operation, operation.0, Vec::new());
    let wall = Duration::from_secs(60);
    let granted = engine.place_one((operation, request), (wall, charge), only, now);
    for (lease, _) in &granted {
        engine.release(Placed {
            lease: *lease,
            operation,
        });
    }
    let _ = engine.sched.forget_queued(operation);
    granted.is_some()
}

/// Asserts, in both directions, that a whole-host booking and a host port exclude each other
/// on a host: a port job is not placed on a host a whole-host allocation holds, and a request
/// for the whole host is not granted while a port job's allocation holds the host, and is
/// once that allocation is freed. Neither direction needs the port check: both follow from
/// a whole-host booking holding every resource of the host, which is the whole host's claim
/// to every port it forwards. It catches a change that lets a port job and a whole-host
/// booking share a host. No pass places a whole-host job yet, so the request is made to the
/// engine's scheduler as a lease of a whole-host class is.
#[test]
fn a_whole_host_job_conflicts_with_every_port_holder() {
    let open = forwarding(&["host-a"], 8080);
    let placer = placer_on(&clock(), &["host-a"]);
    let (_store, jobs) = tables();
    placed(&jobs, "job-w", "host-a", spec(), Booked::WholeHost);
    queue_spec(&jobs, "job-p", ported(&[8080]), 0);
    assert_eq!(pass_with(&placer, &jobs, &open), NONE);
    assert_eq!(state(&jobs, "job-p"), JobState::Queued);

    let placer = placer_on(&clock(), &["host-a"]);
    let (_store, jobs) = tables();
    let a = queue_spec(&jobs, "job-a", ported(&[8080]), 0);
    assert_eq!(pass_with(&placer, &jobs, &open), ["job-a"]);
    let record = jobs.get_job(&a).unwrap().unwrap();
    assert!(!whole_host_granted(&placer, &record, "host-a"));
    finish(&jobs, &a);
    assert_eq!(pass_with(&placer, &jobs, &open), NONE);
    assert!(whole_host_granted(&placer, &record, "host-a"));
}

/// The server's agent side under `--placement js-sched` with the job tables, on a clock the
/// test moves, and `h1` registered with a hello advertising allow-list and the host `ports`.
fn served_with(name: &str, ports: &[u16]) -> (Arc<Agents>, ManualClock, js_testkit::dir::TestDir) {
    let dir = scratch(name);
    let config = config_of(dir.path(), &pool(1, 8000, ""), &["--placement", "js-sched"]);
    let moving = clock();
    let shared = crate::adapt::SharedClock::new(moving.clone());
    let assembled = assemble_placed(config.hosts, config.usage, &shared, &config.state_file);
    let ((_, agents), _) = assembled.unwrap();
    let (_store, jobs) = tables();
    let agents = Arc::try_unwrap(agents.unwrap()).ok().unwrap();
    let agents = agents.with_jobs(Arc::new(jobs));
    say_hello(&agents, ports);
    (Arc::new(agents), moving, dir)
}

/// `h1` says hello advertising allow-list and the host `ports`.
fn say_hello(agents: &Agents, ports: &[u16]) {
    let mut hello = hello_of("h1", 16_000, 32 * GIB);
    ceiling(JobNetwork::AllowList, ports).insert_into(&mut hello.capabilities);
    let credential = Credential::of_token(JOIN);
    agents.registry.hello(&hello, &credential).unwrap();
}

/// Asserts that the server's pass reads the ceiling a host reported in its hello: a job
/// declaring 8080 is placed on `h1` while it advertises 8080; after the host is lost and says
/// hello again advertising 9000, the next pass leaves a job declaring 8080 queued and places
/// one declaring 9000. It catches a ceiling read once and kept, and a pass that offers the
/// hosts without it.
#[test]
fn a_ceiling_change_is_seen_on_the_next_report() {
    let (agents, clock, _dir) = served_with("ports-ceiling", &[8080]);
    let jobs = Arc::clone(agents.jobs.as_ref().unwrap());
    let a = queue_spec(&jobs, "job-a", ported(&[8080]), 0);
    agents.place_jobs();
    assert_eq!(state(&jobs, "job-a"), JobState::Placed);
    finish(&jobs, &a);

    clock.advance(Duration::from_secs(3600));
    say_hello(&agents, &[9000]);
    queue_spec(&jobs, "job-b", ported(&[8080]), 0);
    queue_spec(&jobs, "job-c", ported(&[9000]), 0);
    agents.place_jobs();
    assert_eq!(state(&jobs, "job-b"), JobState::Queued);
    assert_eq!(state(&jobs, "job-c"), JobState::Placed);
}

/// Asserts that a host whose network ceiling is below the level a job asks for is not capable
/// of it: with hosts advertising none and allow-list offered, a job asking for open stays
/// queued, and with a host advertising open offered too it is placed there. It catches a
/// level that is never compared with the ceiling.
#[test]
fn a_host_whose_network_ceiling_is_below_the_job_level_is_not_capable() {
    let placer = placer_on(&clock(), &["host-a", "host-b", "host-c"]);
    let (_store, jobs) = tables();
    let open_job = JobSpec {
        network: NetworkPolicy::Open,
        ..sized(2)
    };
    queue_spec(&jobs, "job-a", open_job, 0);
    let mut hosts = open_with(&["host-a"], &NetworkCeiling::default());
    hosts.extend(open_with(&["host-b"], &ceiling(JobNetwork::AllowList, &[])));
    assert_eq!(pass_with(&placer, &jobs, &hosts), NONE);
    assert_eq!(state(&jobs, "job-a"), JobState::Queued);
    hosts.extend(open_with(&["host-c"], &ceiling(JobNetwork::Open, &[])));
    assert_eq!(pass_with(&placer, &jobs, &hosts), ["job-a"]);
    assert_eq!(on(&jobs, "job-a"), "host-c");
}
