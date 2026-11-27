//! Two real servers over one Postgres database, placing on the `node/<host>` rows (design
//! `multi-instance.md`, G5 and G7): a host that said hello to one server is placed on by the
//! other; a host job, a port, a scale-down mark and the pool's cap are decided in the
//! placing transaction on the rows the store holds, whichever server wrote them. Each server
//! has its own registry, placer and store handle; the hosts say hello through the route, so
//! their rows are written as in production. Every clock is manual or the store's; nothing
//! sleeps, and an interleaving is made by a hook of the store handle or a barrier.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

use axum::Json;
use axum::body::Bytes;
use js_agent_wire::Welcome;
use js_jobs::interfere::Interfere;
use js_jobs::{
    Booked, Capacity, DeclaredPort, JobId, JobSpec, JobState, JobStoreError, NetworkPolicy,
    Placement, ScaleMark,
};
use js_state::Store;
use js_state::postgres::{PostgresConfig, PostgresStore, test_database};
use js_types::Timestamp;
use serde_json::{Value, json};

use super::tests::{host, on_the_machine, queue, queue_spec, sized, state};
use crate::agent_api::job_rig::{Rig, START_MS, placing_rig};
use crate::agent_api::serve_hello;
use crate::agents::Agents;
use crate::job_store::{ServerIds, ServerJobs};

/// One server of the pair: its rig (registry, placer, agent side) with the job tables replaced
/// by a store handle of its own on the shared database, which can run a hook before its next
/// commit.
pub(super) struct Server {
    rig: Rig,
    store: Arc<Interfere<PostgresStore>>,
}

impl Server {
    /// The agent side of the server.
    pub(super) fn agents(&self) -> &Arc<Agents> {
        &self.rig.agents
    }

    /// The server's job tables.
    pub(super) fn jobs(&self) -> &Arc<ServerJobs> {
        &self.rig.jobs
    }
}

/// A server named `name` over the database `url`, as a second process opens it.
pub(super) fn server(name: &str, url: &str) -> Server {
    let mut rig = placing_rig(name);
    let config = PostgresConfig::parse(url).expect("the test URL parses");
    let store = Arc::new(Interfere::new(
        PostgresStore::open(&config).expect("the database opens"),
    ));
    let dynamic: Arc<dyn Store> = store.clone();
    let jobs = js_jobs::JobStore::new(Arc::new(dynamic), ServerIds::random());
    let jobs = Arc::new(jobs.expect("the job store opens"));
    let agents = Arc::try_unwrap(rig.agents).ok().expect("one owner");
    rig.agents = Arc::new(agents.with_jobs(Arc::clone(&jobs)));
    rig.jobs = jobs;
    Server { rig, store }
}

/// `host` says hello to `agents` through the route, reporting `extra` capabilities beside
/// `per-job`, `arch` and `host-jobs`: the welcome, or the refusal as the route's JSON body.
pub(super) fn say_hello(
    agents: &Arc<Agents>,
    host: &str,
    extra: &[(&str, &str)],
) -> Result<Welcome, Value> {
    let mut capabilities = json!({ "per-job": "true", "arch": "x86_64", "host-jobs": "true" });
    for (key, value) in extra {
        capabilities[*key] = json!(value);
    }
    let hello = json!({
        "host_id": host,
        "pool": "burst",
        "agent_version": "0.1.0",
        "shape": {
            "cpu_millis": 8000,
            "memory_bytes": 17179869184u64,
            "gpus": 1,
            "gpu_class": "nvidia-a100",
        },
        "capabilities": capabilities,
        "phase": "serving",
    });
    let body = Bytes::from(serde_json::to_vec(&hello).expect("serializes"));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime");
    let answer = runtime.block_on(serve_hello(agents, host, Ok(body), &|| ()));
    match answer {
        Ok(Json(welcome)) => Ok(welcome),
        Err(refused) => Err(serde_json::to_value(&refused).expect("serializes")),
    }
}

/// The state of the job `id` and the host of its allocation, when it has one.
pub(super) fn placement_of(jobs: &ServerJobs, id: &str) -> (JobState, Option<String>) {
    let parsed = JobId::parse(id).unwrap();
    let allocation = jobs.get_allocation(&parsed).unwrap();
    (state(jobs, id), allocation.map(|a| a.host.to_string()))
}

/// The one-core booking a direct placement uses.
const ONE_CORE: Booked = Booked::Shape {
    cpu_millis: 1000,
    memory_bytes: 1 << 30,
    disk_bytes: 0,
    accelerators: 0,
};

/// What a host of the rigs can hold, as the placing transaction is held to it.
const CAPACITY: Capacity = Capacity {
    cpu_millis: 8800,
    memory_bytes: 16 << 30,
    disk_bytes: 0,
    accelerators: 1,
};

/// Places the queued job `id` on `host` straight through the store handle of `server`, the way
/// a pass does, in the placing transaction.
fn place_directly(server: &Server, id: &str, host_name: &str) -> Result<(), JobStoreError> {
    let id = JobId::parse(id).unwrap();
    let placed = server.jobs().place_claimed(
        &id,
        &host(host_name),
        ONE_CORE,
        &Placement::default(),
        Some(&CAPACITY),
    );
    placed.map(drop)
}

/// A job of one core and 1 GiB at the level allow-list declaring the host port `port`.
fn ported(port: u16) -> JobSpec {
    JobSpec {
        network: NetworkPolicy::AllowList,
        ports: vec![DeclaredPort {
            host: port,
            container: 80,
        }],
        ..sized(1)
    }
}

/// Asserts that a host which said hello to server A alone is placed on by server B's pass, from
/// the row A's hello wrote: B's registry holds no host, two jobs queued in the store are placed
/// by B on A's two hosts, each job once and each with its allocation. It catches a server that
/// offers its placement pass only the hosts whose hellos it took itself, which leaves every
/// host of another server idle while jobs queue.
#[test]
fn a_pass_places_jobs_on_hosts_that_said_hello_to_another_server() {
    let database = test_database();
    let url = database.url();
    let (a, b) = (server("rows_pair_a1", &url), server("rows_pair_b1", &url));
    say_hello(a.agents(), "host-a", &[]).unwrap();
    say_hello(a.agents(), "host-b", &[]).unwrap();
    assert!(b.agents().registry.hosts().is_empty(), "B took no hello");
    // Each job takes more than half a host's memory, so the two do not fit one host.
    queue(b.jobs(), "job-1", 10, 0);
    queue(b.jobs(), "job-2", 10, 0);
    let taints = b.agents().place_jobs();
    assert!(taints.is_empty());
    let one = placement_of(b.jobs(), "job-1");
    let two = placement_of(b.jobs(), "job-2");
    assert_eq!(one.0, JobState::Placed);
    assert_eq!(two.0, JobState::Placed);
    let mut hosts = [one.1.unwrap(), two.1.unwrap()];
    hosts.sort();
    assert_eq!(hosts, ["host-a", "host-b"]);
}

/// The hook that plays server A's pass of a host job on `host-a` just before server B's
/// placing transaction: it queues the host job `job-host` at a higher priority than any job
/// B read, runs A's pass, and notes that it ran.
fn host_job_before_b_commits(a: &Server, b: &Server) -> Arc<AtomicBool> {
    let fired = Arc::new(AtomicBool::new(false));
    let (agents, jobs) = (Arc::clone(a.agents()), Arc::clone(a.jobs()));
    let ran = Arc::clone(&fired);
    b.store.before_next(move || {
        queue_spec(&jobs, "job-host", on_the_machine(1), 10);
        agents.place_jobs();
        ran.store(true, Ordering::SeqCst);
    });
    fired
}

/// Asserts that a contained job server B decided to place on `host-a`, from hosts it read
/// before, is refused by B's pass when server A places a host job on the host in between: A's
/// host job holds the host alone, the contained job stays queued with no allocation. The
/// refusal is the placing transaction's, since B's picture of the host (read before the host
/// job) still said open. It catches a pass that trusts the hosts it read at the start of the
/// pass, which books a job beside a host job that taints the host.
#[test]
fn a_contained_job_is_not_placed_beside_a_host_job_another_server_placed_in_between() {
    let database = test_database();
    let url = database.url();
    let (a, b) = (server("rows_pair_a2", &url), server("rows_pair_b2", &url));
    say_hello(a.agents(), "host-a", &[]).unwrap();
    queue(b.jobs(), "job-contained", 1, 0);
    let fired = host_job_before_b_commits(&a, &b);
    b.agents().place_jobs();
    assert!(
        fired.load(Ordering::SeqCst),
        "A's pass ran before B's commit"
    );
    assert_eq!(
        placement_of(b.jobs(), "job-host"),
        (JobState::Placed, Some("host-a".to_owned()))
    );
    assert_eq!(
        placement_of(b.jobs(), "job-contained"),
        (JobState::Queued, None)
    );
    let taints = b.jobs().host_taints().unwrap();
    assert_eq!(taints[&host("host-a")].job.to_string(), "job-host");
}

/// Asserts that the placement of a contained job on `host-a`, decided before server A placed a
/// host job there, is answered [`JobStoreError::HostTainted`] by the placing transaction of
/// server B, and nothing is written for the job. It catches a placing transaction that checks
/// the host's `taint/` row only for host jobs, or not at all, so that a contained job lands
/// on a host a host job holds.
#[test]
fn the_placing_transaction_refuses_a_host_a_host_job_tainted_in_between() {
    let database = test_database();
    let url = database.url();
    let (a, b) = (server("rows_pair_a3", &url), server("rows_pair_b3", &url));
    say_hello(a.agents(), "host-a", &[]).unwrap();
    queue(b.jobs(), "job-contained", 1, 0);
    let fired = host_job_before_b_commits(&a, &b);
    let refused = place_directly(&b, "job-contained", "host-a");
    assert!(fired.load(Ordering::SeqCst));
    assert!(
        matches!(refused, Err(JobStoreError::HostTainted { .. })),
        "{refused:?}"
    );
    assert_eq!(
        placement_of(b.jobs(), "job-contained"),
        (JobState::Queued, None)
    );
}

/// Places a job declaring the host port 8080 through `first` on `host-a`, which said hello to
/// `first`; then queues a second job declaring it through `second` and asserts that `second`'s
/// pass leaves it queued and that its placing transaction answers
/// [`JobStoreError::PortTaken`] with the port.
fn second_job_wanting_the_port_is_refused(first: &Server, second: &Server) {
    let ceiling = [("egress", "allow-list"), ("egress-ports", "8080")];
    say_hello(first.agents(), "host-a", &ceiling).unwrap();
    queue_spec(first.jobs(), "job-p1", ported(8080), 0);
    assert!(first.agents().place_jobs().is_empty());
    assert_eq!(placement_of(first.jobs(), "job-p1").0, JobState::Placed);
    queue_spec(second.jobs(), "job-p2", ported(8080), 0);
    second.agents().place_jobs();
    assert_eq!(
        placement_of(second.jobs(), "job-p2"),
        (JobState::Queued, None)
    );
    let refused = place_directly(second, "job-p2", "host-a");
    assert!(
        matches!(refused, Err(JobStoreError::PortTaken { port: 8080, .. })),
        "{refused:?}"
    );
}

/// Asserts that a host port a live allocation holds refuses a second job that declares it, from
/// either server: the first job is placed by one server's pass, the other server's pass leaves
/// the second job queued, and its placing transaction answers [`JobStoreError::PortTaken`] with
/// the port; the roles are then swapped on a database of their own. It catches a placement
/// that checks ports against its own picture of the allocations, which two servers each hand
/// the same port to two jobs on one host.
#[test]
fn a_port_a_live_allocation_holds_refuses_a_second_job_from_either_server() {
    let database = test_database();
    let url = database.url();
    let (a, b) = (server("rows_pair_a4", &url), server("rows_pair_b4", &url));
    second_job_wanting_the_port_is_refused(&a, &b);
    let database = test_database();
    let url = database.url();
    let (a, b) = (server("rows_pair_a4x", &url), server("rows_pair_b4x", &url));
    second_job_wanting_the_port_is_refused(&b, &a);
}

/// Asserts that a scale-down mark one server put on a host stops the other from placing there:
/// with `hostmark/host-a` written through A, B's pass does not offer the host, leaves the job
/// queued, and B's placing transaction answers [`JobStoreError::HostMarked`]; when the mark is
/// lifted B's next pass places the job. It catches a server that knows only the marks of its
/// own registry, which places jobs on a host another server is draining.
#[test]
fn a_scale_down_mark_one_server_set_stops_the_other_placing_on_the_host() {
    let database = test_database();
    let url = database.url();
    let (a, b) = (server("rows_pair_a5", &url), server("rows_pair_b5", &url));
    say_hello(a.agents(), "host-a", &[]).unwrap();
    let marked = a
        .jobs()
        .mark_host(&host("host-a"), ScaleMark::Draining, None);
    assert!(marked.unwrap());
    queue(b.jobs(), "job-m", 1, 0);
    let placer = b.agents().placer.as_ref().unwrap();
    let now = Timestamp::from_millis(START_MS);
    let offered = b.agents().offered(b.jobs(), placer, now).unwrap();
    assert!(
        !offered.open.contains_key(&host("host-a")),
        "marked hosts are not offered"
    );
    b.agents().place_jobs();
    assert_eq!(placement_of(b.jobs(), "job-m"), (JobState::Queued, None));
    let refused = place_directly(&b, "job-m", "host-a");
    assert!(
        matches!(refused, Err(JobStoreError::HostMarked { .. })),
        "{refused:?}"
    );

    let (_, version) = a.jobs().read_host_mark(&host("host-a")).unwrap().unwrap();
    assert!(a.jobs().unmark_host(&host("host-a"), version).unwrap());
    b.agents().place_jobs();
    assert_eq!(
        placement_of(b.jobs(), "job-m"),
        (JobState::Placed, Some("host-a".to_owned()))
    );
}

/// Asserts the pool's cap over every server's rows: the pool holds three hosts of a cap of
/// four, all admitted by server A, and A and B each take a hello of a different new host at
/// once (a barrier releases both). Exactly one is admitted and the store holds four rows. The
/// other is refused: `pool-full` when it was judged after the first's row was written, or, when
/// both were judged with room left, `503 state-write-failed` at its row's write, and then
/// `pool-full` at its next hello, before anything of that hello is written. Its registry holds
/// no live host of that name either way. It catches a cap counted over each server's own hosts,
/// which admits both and so a fifth host, and a loser of the race that is admitted on retry.
#[test]
fn two_servers_admit_one_host_into_a_pool_one_below_its_cap() {
    let database = test_database();
    let url = database.url();
    let (a, b) = (server("rows_pair_a6", &url), server("rows_pair_b6", &url));
    for name in ["host-1", "host-2", "host-3"] {
        say_hello(a.agents(), name, &[]).unwrap();
    }
    let barrier = Barrier::new(2);
    let (first, second) = thread::scope(|scope| {
        let one = scope.spawn(|| {
            barrier.wait();
            say_hello(a.agents(), "host-4", &[])
        });
        let two = scope.spawn(|| {
            barrier.wait();
            say_hello(b.agents(), "host-5", &[])
        });
        (one.join().unwrap(), two.join().unwrap())
    });
    let refusals: Vec<&Value> = [&first, &second]
        .into_iter()
        .filter_map(|answer| answer.as_ref().err())
        .collect();
    let lost = [
        json!({ "error": "pool-full" }),
        json!({ "error": "state-write-failed" }),
    ];
    assert!(
        (refusals.len() == 1) && lost.contains(refusals[0]),
        "{first:?} {second:?}"
    );
    let (_, rows) = a.jobs().nodes().unwrap();
    assert_eq!(rows.len(), 4);
    let (loser, name) = match first.is_err() {
        true => (a.agents(), "host-4"),
        false => (b.agents(), "host-5"),
    };
    assert_eq!(
        say_hello(loser, name, &[]),
        Err(json!({ "error": "pool-full" })),
        "the loser's next hello"
    );
    assert_eq!(a.jobs().nodes().unwrap().1.len(), 4);
    // A loser at its row's write keeps the record the registry gave up the session of
    // (`Registry::abandon`), never heard, so it is no host the pass offers.
    let live: Vec<String> = loser
        .registry
        .hosts()
        .iter()
        .filter(|view| view.state == js_registry::HostState::Live)
        .map(|view| view.id.to_string())
        .collect();
    assert!(!live.iter().any(|live| live == name), "{live:?}");
}
