//! The queued jobs placed by the engine: queue order, a queue that needs no presentation, the
//! commit before the start, a restart between the two, what frees a booking, the share job
//! that waits by design, jobs beside leases on one host, the jobs a pass does not place,
//! the engine's own events, and the server's pass. Every clock is manual; nothing sleeps.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use js_agent_wire::{Command, HeldJob, HeldState, NetworkCeiling};
use js_jobs::interfere::Interfere;
use js_jobs::{AllocationStep, JobEnd, JobSpec, JobState, NewJob, Output};
use js_lease::ManualClock;
use js_provider::{Candidate, Pool};
use js_registry::{Credential, Placer};
use js_state::Store;
use js_state::fake::Fake;
use js_types::{LeaseId, Shape};

use super::*;
use crate::adapt::SharedClock;
use crate::agent_api::job_rig::{placed, spec, tables};
use crate::agents::assemble_placed;
use crate::job_alloc::{Freed, free_allocation};
use crate::per_job::{BeatContext, serve};
use crate::sched::engine::Settings;
use crate::sched::engine_rig::{GIB, T0, hello_of, pool, rig, share_of};
use crate::sched::wiring_tests::{JOIN, config_of, scratch};

/// The host `name`.
pub(super) fn host(name: &str) -> HostId {
    HostId::new(name).unwrap()
}

/// A host of 8 cores and 16 GiB, with an `arch`.
pub(super) fn shape() -> Shape {
    let caps = [("arch".to_owned(), "x86_64".to_owned())];
    Shape::new(
        Resources::new(8000, 16 * GIB, 0, None).unwrap(),
        caps.into_iter().collect(),
    )
}

/// A clock at [`T0`].
pub(super) fn clock() -> ManualClock {
    ManualClock::new(Timestamp::from_millis(T0))
}

/// A placer on `clock` whose engine was told of `hosts` saying hello, each of [`shape`].
pub(super) fn placer_on(clock: &ManualClock, hosts: &[&str]) -> SchedPlacer {
    let placer = SchedPlacer::new(Settings::default(), SharedClock::new(clock.clone()));
    for name in hosts {
        placer.host_up(&host(name), &shape());
    }
    placer
}

/// The hosts `names`, each advertising no network and no port.
pub(super) fn open(names: &[&str]) -> BTreeMap<HostId, NetworkCeiling> {
    names
        .iter()
        .map(|name| (host(name), NetworkCeiling::default()))
        .collect()
}

/// A job asking for one core and `gib` GiB.
pub(super) fn sized(gib: u64) -> JobSpec {
    JobSpec {
        needs: Needs::Shape(Resources::new(1000, gib * GIB, 0, None).unwrap()),
        ..spec()
    }
}

/// A booking of one core and `gib` GiB.
pub(super) fn booked(gib: u64) -> Booked {
    Booked::Shape {
        cpu_millis: 1000,
        memory_bytes: gib * GIB,
        disk_bytes: 0,
        accelerators: 0,
    }
}

/// Records the job `id`, queued with `spec` at `priority`.
pub(super) fn queue_spec(jobs: &ServerJobs, id: &str, spec: JobSpec, priority: i32) -> JobId {
    let new = NewJob {
        id: Some(JobId::parse(id).unwrap()),
        spec,
        priority,
        client: None,
    };
    jobs.insert_queued(new).unwrap().record.id
}

/// Records the job `id`, queued asking for `gib` GiB at `priority`.
pub(super) fn queue(jobs: &ServerJobs, id: &str, gib: u64, priority: i32) -> JobId {
    queue_spec(jobs, id, sized(gib), priority)
}

/// The state of the job `id`.
pub(super) fn state(jobs: &ServerJobs, id: &str) -> JobState {
    let id = JobId::parse(id).unwrap();
    jobs.get_job(&id).unwrap().unwrap().state
}

/// Runs the placed job `id` to its exit.
pub(super) fn run_to_exit(jobs: &ServerJobs, id: &JobId) {
    jobs.transition(id, JobState::Placed, JobState::Starting)
        .unwrap();
    jobs.transition(id, JobState::Starting, JobState::Running)
        .unwrap();
    let exited = JobEnd::Exited {
        exit_code: 0,
        output: Output::default(),
    };
    jobs.end_job(id, exited).unwrap();
}

/// Runs the placed job `id` to its exit and frees its allocation on its host's confirmation.
pub(super) fn finish(jobs: &ServerJobs, id: &JobId) {
    run_to_exit(jobs, id);
    free_allocation(jobs, id, Freed::HostConfirmed).unwrap();
}

/// One pass of `placer` over `jobs` on the hosts `names`: the jobs it placed.
pub(super) fn pass(placer: &SchedPlacer, jobs: &ServerJobs, names: &[&str]) -> Vec<String> {
    let placed = placer.place_jobs(jobs, &open(names), &NoStarts).unwrap();
    placed.iter().map(ToString::to_string).collect()
}

/// No job placed.
pub(super) const NONE: [&str; 0] = [];

/// Asserts that when the only host frees, the front job that fits it gets it and the job
/// behind it stays queued, though it alone would have fitted the room left before the host
/// freed. It catches a walk that lets a later job take a host the front one waits for, or
/// that places the later job first (section 8, row 11).
#[test]
fn queue_order_is_binding_when_a_host_frees() {
    let placer = placer_on(&clock(), &["host-a"]);
    let (_store, jobs) = tables();
    let (fill, _) = placed(&jobs, "job-fill", "host-a", sized(8), booked(8));
    queue(&jobs, "job-a", 10, 0);
    queue(&jobs, "job-b", 8, 0);
    assert_eq!(pass(&placer, &jobs, &["host-a"]), NONE);
    assert_eq!(
        (state(&jobs, "job-a"), state(&jobs, "job-b")),
        (JobState::Queued, JobState::Queued)
    );
    finish(&jobs, &fill.id);
    assert_eq!(pass(&placer, &jobs, &["host-a"]), ["job-a"]);
    assert_eq!(state(&jobs, "job-b"), JobState::Queued);
}

/// Asserts that a job queued an hour before any room appears, with no event naming it, is
/// placed when the host frees, ahead of a job that arrived later (whose id sorts first).
/// It catches an engine that drops a job not presented within a grace, or that puts it
/// back behind later arrivals (section 8, row 12).
#[test]
fn a_queued_job_needs_no_re_presentation() {
    let clock = clock();
    let placer = placer_on(&clock, &["host-a"]);
    let time = Arc::new(js_state::ManualClock::default());
    let store = Arc::new(Interfere::new(Fake::with_flaw(
        Arc::clone(&time),
        js_state::fake::Flaw::None,
    )));
    let dynamic: Arc<dyn Store> = store;
    let jobs = js_jobs::JobStore::new(Arc::new(dynamic), js_jobs::RandomIds).unwrap();
    let (fill, _) = placed(&jobs, "job-fill", "host-a", sized(8), booked(8));
    queue(&jobs, "job-z-first", 10, 0);
    assert_eq!(pass(&placer, &jobs, &["host-a"]), NONE);
    let hour = Duration::from_secs(3600);
    clock.advance(hour);
    time.advance(hour);
    queue(&jobs, "job-a-later", 10, 0);
    assert_eq!(pass(&placer, &jobs, &["host-a"]), NONE);
    finish(&jobs, &fill.id);
    assert_eq!(pass(&placer, &jobs, &["host-a"]), ["job-z-first"]);
    assert_eq!(state(&jobs, "job-a-later"), JobState::Queued);
}

/// What a start saw of the store when it was handed on.
#[derive(Debug, PartialEq, Eq)]
struct Seen {
    job: String,
    host: String,
    /// The transactions committed by then.
    commits: u64,
    /// The job's state and its allocation's step by then.
    state: JobState,
    step: Option<AllocationStep>,
}

/// A [`JobStarts`] that records what the store held at each start.
struct Recorder<'a> {
    jobs: &'a ServerJobs,
    store: &'a Interfere<Fake>,
    seen: Mutex<Vec<Seen>>,
}

impl JobStarts for Recorder<'_> {
    fn placed(&self, job: &JobId, host: &HostId) {
        let record = self.jobs.get_job(job).unwrap().unwrap();
        let allocation = self.jobs.get_allocation(job).unwrap();
        self.seen.lock().unwrap().push(Seen {
            job: job.to_string(),
            host: host.to_string(),
            commits: self.store.commit_stats().txns,
            state: record.state,
            step: allocation.map(|allocation| allocation.step),
        });
    }
}

/// Asserts that a placement is one transaction holding the job's move to placed and its
/// booked allocation, and that the start is handed on only after it, with both rows there;
/// and that a failed commit leaves the job queued with no allocation and no start, lets
/// its booking go in the engine, and the next pass places it. It catches the allocation
/// written in a second transaction, a start before the commit, and a failed commit that
/// keeps its room booked.
#[test]
fn placement_and_allocation_are_one_commit() {
    let placer = placer_on(&clock(), &["host-a"]);
    let (store, jobs) = tables();
    let starts = Recorder {
        jobs: &jobs,
        store: &store,
        seen: Mutex::new(Vec::new()),
    };
    queue(&jobs, "job-a", 4, 0);
    let before = store.commit_stats().txns;
    let placed = placer.place_jobs(&jobs, &open(&["host-a"]), &starts);
    assert_eq!(placed, Ok(vec![JobId::parse("job-a").unwrap()]));
    assert_eq!(store.commit_stats().txns, before + 1);
    let seen = |job: &str, commits| Seen {
        job: job.to_owned(),
        host: "host-a".to_owned(),
        commits,
        state: JobState::Placed,
        step: Some(AllocationStep::Booked),
    };
    assert_eq!(*starts.seen.lock().unwrap(), [seen("job-a", before + 1)]);

    queue(&jobs, "job-b", 4, 0);
    store.fail_next();
    let refused = placer.place_jobs(&jobs, &open(&["host-a"]), &starts);
    assert_eq!(refused, Ok(Vec::new()));
    let b = JobId::parse("job-b").unwrap();
    assert_eq!(
        (state(&jobs, "job-b"), jobs.get_allocation(&b).unwrap()),
        (JobState::Queued, None)
    );
    assert_eq!(starts.seen.lock().unwrap().len(), 1);
    let booked = |placer: &SchedPlacer| placer.booked_on("host-a").unwrap().memory_bytes;
    assert_eq!(booked(&placer), 4 * GIB);
    let commits = store.commit_stats().txns;
    assert_eq!(pass(&placer, &jobs, &["host-a"]), ["job-b"]);
    assert_eq!(booked(&placer), 8 * GIB);
    assert_eq!(starts.seen.lock().unwrap().len(), 1);
    assert_eq!(store.commit_stats().txns, commits + 1);
}

/// Asserts that after a placement is committed and the server stops before its start goes
/// out, a new engine over the same tables books the allocation before it places anything
/// (its room is not offered to the next job) and does not place the job again; that the
/// host listing the job keeps it booked with no start, and a host whose listing after its
/// hello is clean is sent the start. It catches a booking forgotten at a restart and a
/// placement made twice.
#[test]
fn a_crash_between_placement_and_the_host_call_resolves_by_observation() {
    let (_store, jobs) = tables();
    let clock = clock();
    let first = placer_on(&clock, &["host-a"]);
    queue(&jobs, "job-a", 10, 0);
    assert_eq!(pass(&first, &jobs, &["host-a"]), ["job-a"]);
    let a = JobId::parse("job-a").unwrap();
    let allocation = jobs.get_allocation(&a).unwrap().unwrap();
    drop(first);

    let second = placer_on(&clock, &["host-a"]);
    queue(&jobs, "job-b", 10, 0);
    assert_eq!(pass(&second, &jobs, &["host-a"]), NONE);
    assert_eq!(second.booked_on("host-a").unwrap().memory_bytes, 10 * GIB);
    assert_eq!(state(&jobs, "job-a"), JobState::Placed);
    assert_eq!(jobs.get_allocation(&a).unwrap(), Some(allocation));

    let ceiling = NetworkCeiling::default();
    let context = BeatContext {
        host: &host("host-a"),
        ceiling: &ceiling,
        logs: None,
    };
    let holds = [HeldJob {
        job_id: "job-a".to_owned(),
        state: HeldState::Starting,
        result: None,
    }];
    let clean = serve(&jobs, &context, &[], &[], 8).unwrap();
    assert_eq!(clean.commands.len(), 1);
    assert!(matches!(clean.commands[0], Command::JobStart(_)));
    assert!(format!("{:?}", clean.commands[0]).contains("job_id: \"job-a\""));
    let listed = serve(&jobs, &context, &holds, &[], 8).unwrap();
    assert!(listed.commands.is_empty());
    let step = jobs.get_allocation(&a).unwrap().unwrap().step;
    assert_eq!(step, AllocationStep::Booked);
    assert_eq!(pass(&second, &jobs, &["host-a"]), NONE);
}

/// What a test says happened to the host of an ended job's allocation.
#[derive(Debug, Clone, Copy)]
enum Evidence {
    /// The host confirmed the container gone.
    HostConfirmed,
    /// The provider read the host absent.
    ProviderAbsent,
    /// The provider answered unknown.
    ProviderUnknown,
    /// The host said nothing, for longer than the engine's own grace.
    HostSilent,
    /// The observation failed.
    ObservationError,
    /// The host still lists the job.
    StillListed,
}

/// Asserts, for each thing the server can learn of the host of an ended job, that only the
/// host's confirmation and the provider's absence free its allocation and its room: an
/// unknown answer, a failed observation and a host silent past the engine's grace (which
/// makes the scheduler give its lease up) leave it releasing and booked, and so does a host
/// that still lists the job. It catches a free on unknown or on silence, in the table or in
/// the engine.
#[test]
fn only_absent_frees_an_allocation() {
    use Evidence::*;
    let cases = [
        (HostConfirmed, true),
        (ProviderAbsent, true),
        (ProviderUnknown, false),
        (HostSilent, false),
        (ObservationError, false),
        (StillListed, false),
    ];
    for (evidence, frees) in cases {
        let clock = clock();
        let placer = placer_on(&clock, &["host-a"]);
        let (_store, jobs) = tables();
        let a = queue(&jobs, "job-a", 10, 0);
        assert_eq!(pass(&placer, &jobs, &["host-a"]), ["job-a"]);
        run_to_exit(&jobs, &a);
        queue(&jobs, "job-b", 10, 0);
        match evidence {
            HostConfirmed => {
                free_allocation(&jobs, &a, Freed::HostConfirmed).unwrap();
            }
            ProviderAbsent => {
                free_allocation(&jobs, &a, Freed::ProviderAbsent).unwrap();
            }
            // No evidence: there is no `Freed` for it, so nothing is freed.
            ProviderUnknown | ObservationError => {}
            HostSilent => {
                clock.advance(Duration::from_secs(600));
                assert_eq!(pass(&placer, &jobs, &[]), NONE);
            }
            StillListed => {
                let ceiling = NetworkCeiling::default();
                let context = BeatContext {
                    host: &host("host-a"),
                    ceiling: &ceiling,
                    logs: None,
                };
                let holds = [HeldJob {
                    job_id: "job-a".to_owned(),
                    state: HeldState::ExitedUnacked,
                    result: None,
                }];
                serve(&jobs, &context, &holds, &[], 8).unwrap();
            }
        }
        let placed = pass(&placer, &jobs, &["host-a"]);
        let step = jobs.get_allocation(&a).unwrap().unwrap().step;
        let freed = step == AllocationStep::Freed;
        assert_eq!(
            (placed == ["job-b"], freed),
            (frees, frees),
            "{evidence:?}: {step:?}"
        );
        if !frees {
            assert_eq!(step, AllocationStep::Releasing, "{evidence:?}");
        }
    }
}

/// Asserts that a large share job at the front of its priority, on a host a stream of more
/// urgent small jobs never leaves room on, is given no mark or hold, stops nothing that
/// runs, holds back the small job queued behind it (which alone would fit), and is placed
/// the first time there is room for it, as the walk's rustdoc and `docs/design.md` say. It
/// catches a mark or a preemption for a share job, and a small job behind it placed on the
/// room it waits for (section 8, row 17).
#[test]
fn a_large_share_job_among_a_steady_small_stream_is_documented_not_starved_by_design() {
    let placer = placer_on(&clock(), &["host-a"]);
    let (_store, jobs) = tables();
    let mut running: Vec<JobId> = (0..3)
        .map(|n| {
            let id = format!("job-run-{n}");
            placed(&jobs, &id, "host-a", sized(4), booked(4)).0.id
        })
        .collect();
    queue(&jobs, "job-large", 10, 0);
    queue(&jobs, "job-small", 2, 0);
    for round in 0..3 {
        finish(&jobs, &running.remove(0));
        let urgent = queue(&jobs, &format!("job-urgent-{round}"), 4, 5);
        assert_eq!(pass(&placer, &jobs, &["host-a"]), [urgent.to_string()]);
        running.push(urgent);
        assert_eq!(
            (state(&jobs, "job-large"), state(&jobs, "job-small")),
            (JobState::Queued, JobState::Queued),
            "round {round}"
        );
        assert_eq!(
            placer.waiting(),
            0,
            "round {round}: a hold for a queued job"
        );
        let live = jobs.live_allocations().unwrap();
        let booked = live.iter().filter(|a| a.step == AllocationStep::Booked);
        assert_eq!(
            booked.count(),
            3,
            "round {round}: a running job was stopped"
        );
    }
    for id in &running {
        finish(&jobs, id);
    }
    assert_eq!(
        pass(&placer, &jobs, &["host-a"]),
        ["job-large", "job-small"]
    );
}

/// Asserts that on one host a lease and a job share the room without overbooking: with a
/// lease and a job placed, a second job and a second lease that need more than is left both
/// wait or are refused; stopping the lease frees its room in the engine at once, before any
/// pass, and the next pass gives exactly that room to the front job. It catches an engine
/// that counts only job allocations or only lease bookings (section 8, row 25).
#[test]
fn lease_and_job_bookings_cannot_overbook_a_host() {
    let sched = ["--placement", "js-sched"];
    let mut rig = rig("jobs-beside-leases", &pool(1, 8000, ""), &["h1"], &sched);
    let (_store, jobs) = tables();
    let on_h1 = open(&["h1"]);
    let memory =
        |rig: &crate::sched::engine_rig::Rig| rig.placer().booked_on("h1").unwrap().memory_bytes;
    assert_eq!(rig.start(share_of(1, 1000, 6 * GIB)).as_deref(), Some("h1"));
    queue(&jobs, "job-1", 6, 0);
    let placed = |rig: &crate::sched::engine_rig::Rig| {
        let placed = rig.placer().place_jobs(&jobs, &on_h1, &NoStarts).unwrap();
        placed.iter().map(ToString::to_string).collect::<Vec<_>>()
    };
    assert_eq!(placed(&rig), ["job-1"]);
    queue(&jobs, "job-2", 6, 0);
    assert_eq!(placed(&rig), NONE);
    assert_eq!(rig.start(share_of(2, 1000, 6 * GIB)), None);
    assert_eq!(memory(&rig), 12 * GIB);
    rig.provider.stop(&LeaseId::new(1, 1));
    assert_eq!(memory(&rig), 6 * GIB);
    queue(&jobs, "job-3", 5, 0);
    assert_eq!(placed(&rig), ["job-2"]);
    assert_eq!(state(&jobs, "job-3"), JobState::Queued);
    assert_eq!(memory(&rig), 12 * GIB);
}

/// Asserts that a job that books the whole host, and one whose constraints are no platform
/// requirement, are not placed and hold no host back from the jobs behind them; that a job
/// naming its `arch` is placed on a host of that arch; and that a live allocation of the
/// whole host books it whole. It catches a pass that places a whole-host job as a share,
/// that stops at a job it cannot describe, or that offers the room of a host held whole.
#[test]
fn a_whole_host_job_or_one_no_host_could_describe_holds_back_no_one() {
    let placer = placer_on(&clock(), &["host-a", "host-b"]);
    let (_store, jobs) = tables();
    let whole = JobSpec {
        needs: Needs::WholeHost,
        ..spec()
    };
    queue_spec(&jobs, "job-1-whole", whole, 9);
    let mut odd = sized(2);
    odd.constraints.capabilities = BTreeMap::from([("flavour".to_owned(), "x".to_owned())]);
    queue_spec(&jobs, "job-2-odd", odd, 8);
    let mut pinned = sized(2);
    pinned.constraints.arch = Some("x86_64".to_owned());
    queue_spec(&jobs, "job-3-arch", pinned, 7);
    assert_eq!(pass(&placer, &jobs, &["host-a"]), ["job-3-arch"]);
    assert_eq!(
        (state(&jobs, "job-1-whole"), state(&jobs, "job-2-odd")),
        (JobState::Queued, JobState::Queued)
    );

    placed(&jobs, "job-4-held", "host-b", spec(), Booked::WholeHost);
    queue(&jobs, "job-5", 2, 0);
    assert_eq!(pass(&placer, &jobs, &["host-b"]), NONE);
    assert_eq!(state(&jobs, "job-5"), JobState::Queued);
}

/// Asserts that the engine keeps a job's booking through its own events: an engine built
/// again (when it disagrees with the registry) carries it, a heartbeat of its host lists
/// its lease so the scheduler keeps it past a placement round, and a host the registry
/// forgets is kept as a worker while it holds one. It catches a rebuild that drops the job
/// allocations, a heartbeat that gives them up, and a forgotten host taking its bookings
/// with it.
#[test]
fn the_engine_keeps_a_jobs_booking_through_its_own_events() {
    let clock = clock();
    let placer = placer_on(&clock, &["host-a"]);
    let (_store, jobs) = tables();
    queue(&jobs, "job-a", 10, 0);
    assert_eq!(pass(&placer, &jobs, &["host-a"]), ["job-a"]);
    let a = host("host-a");
    let worker = map::worker(&a);
    let every = [Candidate {
        host: a.clone(),
        shape: shape(),
        leases: Vec::new(),
    }];
    let now = placer.now();
    let mut rebuilt = placer.engine().rebuild(Settings::default(), &every, now);
    let memory = |engine: &Engine| engine.sched.booked(&worker).unwrap().memory_bytes;
    assert_eq!(memory(&rebuilt), 10 * GIB);
    let later = FarmTime::from_millis(now.as_millis() + 5_000);
    rebuilt.heard(&a, None, later);
    rebuilt.feed(later, Event::Tick);
    assert_eq!(rebuilt.job_leases_on(&a).count(), 1);
    let held = rebuilt.job_leases_on(&a).collect::<Vec<_>>();
    assert_eq!(rebuilt.sched.leases_on(&worker), held);
    rebuilt.host_gone(&a);
    assert_eq!(memory(&rebuilt), 10 * GIB);
    rebuilt.job_failed(&JobId::parse("job-a").unwrap());
    assert_eq!(memory(&rebuilt), 0);
    rebuilt.host_gone(&a);
    assert!(rebuilt.sched.capacity(&worker).is_none());
}

/// Asserts that a pass whose read of either table fails answers the error and places
/// nothing. It catches a failed read taken as an empty table.
#[test]
fn a_pass_that_cannot_read_the_tables_places_nothing() {
    let placer = placer_on(&clock(), &["host-a"]);
    let (store, jobs) = tables();
    queue(&jobs, "job-a", 2, 0);
    for reads in [0, 1] {
        store.fail_read_after(reads);
        let failed = placer.place_jobs(&jobs, &open(&["host-a"]), &NoStarts);
        assert!(failed.is_err(), "the read after {reads}");
        assert_eq!(state(&jobs, "job-a"), JobState::Queued);
    }
}

/// The agent side of a server under `--placement js-sched` holding job tables over a store
/// that can fail, with `h1` registered, and the store and directory it runs on.
pub(super) fn served(name: &str) -> (Arc<Agents>, Arc<Interfere<Fake>>, js_testkit::dir::TestDir) {
    let dir = scratch(name);
    let config = config_of(dir.path(), &pool(1, 8000, ""), &["--placement", "js-sched"]);
    let shared = SharedClock::new(clock());
    let assembled = assemble_placed(
        config.hosts,
        config.usage,
        &shared,
        &config.state_file,
        None,
    );
    let ((_, agents), _) = assembled.unwrap();
    let (store, jobs) = tables();
    let agents = Arc::try_unwrap(agents.unwrap()).ok().unwrap();
    let agents = agents.with_jobs(Arc::new(jobs));
    let hello = hello_of("h1", 16_000, 32 * GIB);
    agents
        .registry
        .hello(&hello, &Credential::of_token(JOIN))
        .unwrap();
    (Arc::new(agents), store, dir)
}

/// Asserts that the server's pass places a queued job on a host the registry lists live
/// and serving, offers no job to a cordoned host, logs a failed read and places nothing,
/// and places the job once the cordon is lifted. It catches a pass that offers a host the
/// registry would not, or that is not run at all under `--placement js-sched`.
#[test]
fn the_server_places_queued_jobs_on_the_hosts_the_registry_opens() {
    let (agents, store, _dir) = served("jobs-served");
    let jobs = Arc::clone(agents.jobs.as_ref().unwrap());
    queue(&jobs, "job-a", 2, 0);
    agents.place_jobs();
    let a = JobId::parse("job-a").unwrap();
    assert_eq!(state(&jobs, "job-a"), JobState::Placed);
    assert_eq!(jobs.get_allocation(&a).unwrap().unwrap().host, host("h1"));

    agents
        .registry
        .cordon(&host("h1"), Duration::from_secs(600))
        .unwrap();
    queue(&jobs, "job-b", 2, 0);
    agents.place_jobs();
    assert_eq!(state(&jobs, "job-b"), JobState::Queued);
    agents.registry.uncordon(&host("h1")).unwrap();
    store.fail_read_after(0);
    agents.place_jobs();
    assert_eq!(state(&jobs, "job-b"), JobState::Queued);
    agents.place_jobs();
    assert_eq!(state(&jobs, "job-b"), JobState::Placed);
}

/// Asserts that a placement commit that fails for an ordinary reason leaves the pass going
/// (the next job is placed) and the fence untripped, and that a commit the store answers with
/// `Fenced` ends the pass at that job, places nothing after it and trips the fence. It catches
/// a fenced store let go like a failed commit, so a server that is no longer the writer goes on
/// offering jobs.
#[test]
fn a_fenced_placement_commit_ends_the_pass_and_trips_the_fence() {
    let (agents, store, _dir) = served("jobs-fenced-commit");
    let jobs = Arc::clone(agents.jobs.as_ref().unwrap());
    queue(&jobs, "job-a", 2, 0);
    queue(&jobs, "job-b", 2, 0);
    store.fail_next();
    agents.place_jobs();
    assert!(!agents.fence.is_tripped());
    assert_eq!(
        (state(&jobs, "job-a"), state(&jobs, "job-b")),
        (JobState::Queued, JobState::Placed)
    );

    queue(&jobs, "job-c", 2, 0);
    queue(&jobs, "job-d", 2, 0);
    store.fail_with(js_state::StoreError::Fenced);
    store.fail_next();
    agents.place_jobs();
    assert!(agents.fence.is_tripped());
    assert_eq!(
        ["job-a", "job-c", "job-d"].map(|job| state(&jobs, job)),
        [JobState::Queued; 3]
    );
}

/// Asserts that a read of the tables the store answers with `Fenced` trips the fence, and
/// places nothing. It catches a fenced read logged as a failed one.
#[test]
fn a_fenced_read_of_the_queue_trips_the_fence() {
    let (agents, store, _dir) = served("jobs-fenced-read");
    let jobs = Arc::clone(agents.jobs.as_ref().unwrap());
    queue(&jobs, "job-a", 2, 0);
    store.fail_with(js_state::StoreError::Fenced);
    store.fail_read_after(0);
    agents.place_jobs();
    assert!(agents.fence.is_tripped());
    assert_eq!(state(&jobs, "job-a"), JobState::Queued);
}
