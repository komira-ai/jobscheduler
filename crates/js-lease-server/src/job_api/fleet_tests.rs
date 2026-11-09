//! The fleet calls over the server's own state: a registry on a manual clock with hosts
//! that said hello, the lease table over it, the agent side that saves the bindings into a
//! scratch directory, and the job tables over the in-memory store on a manual clock of its
//! own. The pools and the history come from the server's own status, usage sink and job
//! store, or from fakes where a test needs a view the server would take a provider to make. Nothing
//! here opens a socket or sleeps: gRPC runs over an in-memory stream.

use std::collections::{BTreeMap, BTreeSet};
use std::future::{Ready, ready};
use std::io;
use std::path::Path;
use std::task::{Context, Poll};

use axum::http::Uri;
use hyper_util::rt::TokioIo;
use js_agent_wire::{Hello, Phase as AgentPhase, Taint};
use js_jobs::interfere::Interfere;
use js_jobs::{DeclaredPort, EndReason, JobSpec, JobStore, Needs};
use js_lease::{Boot, LeaseTable, ManualClock, Settings, Timestamp};
use js_proto::FILE_DESCRIPTOR_SET;
use js_provider::{HostCapacity, Pool as _, RefusalCode, StartAck, StartSpec};
use js_registry::{Credential, HostView, PoolConfig, Registry, SequentialSessions};
use js_scaler::Phase;
use js_state::Store;
use js_state::fake::{Fake, Flaw};
use js_testkit::dir::TestDir;
use js_types::{LeaseId, MemoryHeadroom, ProviderId, Request as LeaseRequest};
use js_types::{ResourceClass, Resources, Shape};
use js_usage::{Booked as UsageBooked, Used};
use prost_types::FileDescriptorSet;
use serde_json::{Value, json};
use tokio::io::DuplexStream;
use tonic::Code;
use tonic::codegen::tokio_stream;
use tonic::transport::{Channel, Endpoint, Server as GrpcServer};
use tonic_prost::prost::Message;
use tower::Service;

use super::*;
use crate::adapt::{BoxedPool, SharedClock};
use crate::agent_api::job_rig::{BOOKED, placed, spec};
use crate::agents::Agents;
use crate::config::parse_token;
use crate::job_store::ServerJobs;
use crate::pool::{NextLaunch, NoOffer};
use crate::status::{Instances, LaunchView, OrphanView};
use crate::usage_js::{NodeUsage, no_names};

#[path = "fleet_hosts_tests.rs"]
mod hosts;
#[path = "fleet_pools_tests.rs"]
mod pools;
#[path = "fleet_providers_tests.rs"]
mod providers;
#[path = "fleet_usage_tests.rs"]
mod usage;

/// When the registry's clock starts, in milliseconds since the Unix epoch.
const T0: u64 = 1_800_000_000_000;

/// The shape the pool books and every host says hello with: 8 cores, 16 GiB, one GPU.
fn resources() -> Resources {
    Resources::new(8000, 16 << 30, 1, Some("nvidia-a100".to_owned())).unwrap()
}

/// The server under test: its shared state, the job service over it, and the handles a
/// test reads the state through.
struct Server {
    shared: Arc<Shared>,
    api: JobApi,
    registry: Registry,
    clock: ManualClock,
    store_clock: Arc<js_state::ManualClock>,
    store: Arc<Interfere<Fake>>,
    jobs: Arc<ServerJobs>,
    usage: Arc<Fake>,
    sink: Arc<NodeUsage>,
    dir: TestDir,
}

/// A server named `name` with the agent side when `agents` (a server without it holds only
/// hosts no agent registered).
fn server(name: &str, agents: bool) -> Server {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/fleet_calls");
    let dir = TestDir::under(root, name);
    let clock = ManualClock::new(Timestamp::from_millis(T0));
    let shared_clock = SharedClock::new(clock.clone());
    let shape = Shape::new(
        resources(),
        BTreeMap::from([("arch".to_string(), "x86_64".to_string())]),
    );
    let pool = PoolConfig::new(
        ProviderId::new("burst").unwrap(),
        ResourceClass::gpu_host(),
        shape,
        4,
    );
    let registry = Registry::with_sessions(
        pool,
        clock.clone(),
        SequentialSessions::new(),
        Arc::new(js_sched_placer::place::SchedPlacer::default()),
    );
    let settings = Settings::new();
    let table = LeaseTable::open(
        BoxedPool::new(Box::new(registry.provider())),
        shared_clock.clone(),
        settings,
        dir.path().join("leases.toml"),
        Boot::Initialise,
    )
    .unwrap();
    let store_clock = Arc::new(js_state::ManualClock::default());
    let store = Arc::new(Interfere::new(Fake::with_flaw(
        store_clock.clone(),
        Flaw::None,
    )));
    let dynamic: Arc<dyn Store> = store.clone();
    let job_history: Arc<dyn History> = Arc::new(StoreHistory(dynamic.clone()));
    let jobs =
        Arc::new(JobStore::new(Arc::new(dynamic), crate::job_store::ServerIds::random()).unwrap());
    let side = Agents::over(registry.clone(), &shared_clock, dir.path()).with_jobs(jobs.clone());
    let token = parse_token(&[b'c'; 40]).unwrap();
    let shared = Arc::new(Shared::new(table, token, agents.then(|| Arc::new(side))));
    let usage = Arc::new(Fake::with_flaw(Arc::default(), Flaw::None));
    let usage_store: Arc<dyn Store> = usage.clone();
    let sink = Arc::new(NodeUsage::over(usage_store, 1 << 20, 0, no_names()).unwrap());
    let status = PoolStatus::new(None, None, Vec::new(), shared_clock.clone());
    let history: Arc<dyn History> = Arc::new(SinkHistory(sink.clone()));
    let fleet = Ctx::served(shared.clone(), Arc::new(status), Some(history))
        .with_job_history(Some(job_history));
    let api = JobApi::new(jobs.clone(), shared_clock).with_fleet(fleet);
    Server {
        shared,
        api,
        registry,
        clock,
        store_clock,
        store,
        jobs,
        usage,
        sink,
        dir,
    }
}

impl Server {
    /// Says hello for `host`, serving, with the pool's shape.
    fn register(&self, host: &str) {
        let hello: Hello = serde_json::from_value(json!({
            "host_id": host,
            "pool": "burst",
            "agent_version": "0.1.0",
            "shape": {
                "cpu_millis": 8000,
                "memory_bytes": 17179869184u64,
                "gpus": 1,
                "gpu_class": "nvidia-a100",
            },
            "capabilities": { "arch": "x86_64" },
            "phase": "serving",
        }))
        .unwrap();
        self.registry
            .hello(&hello, &Credential::of_token(host))
            .unwrap();
    }

    /// Asks the registry for a lease of sequence `n` through the provider the lease routes
    /// hold.
    fn lease(&self, n: u64) -> StartAck {
        let request = LeaseRequest {
            class: ResourceClass::gpu_host().id().clone(),
            resources: resources(),
            capabilities: Default::default(),
            max_wall: Duration::from_secs(600),
            labels: Default::default(),
            validation_run: None,
            probe_host: None,
        };
        let spec = StartSpec::new(LeaseId::new(1, n), request, &[ResourceClass::gpu_host()]);
        self.registry.provider().start(spec.unwrap())
    }

    /// The bindings file the agent side writes, if it has written one.
    fn bindings_file(&self) -> Option<String> {
        std::fs::read_to_string(self.dir.path().join("bindings")).ok()
    }
}

fn started(host: &str) -> StartAck {
    StartAck::Started {
        host: HostId::new(host).unwrap(),
    }
}

async fn list(api: &JobApi) -> Vec<wire::Host> {
    let listed = list_hosts(api, Request::new(ListHostsRequest {})).await;
    listed.unwrap().into_inner().hosts
}

fn find<'a>(hosts: &'a [wire::Host], id: &str) -> &'a wire::Host {
    hosts.iter().find(|host| host.id == id).unwrap()
}

async fn cordon(api: &JobApi, host: &str, secs: Option<u32>) -> Result<CordonHostResponse, Status> {
    let request = CordonHostRequest {
        host_id: host.to_owned(),
        cordon_secs: secs,
    };
    let answer = cordon_host(api, Request::new(request)).await;
    answer.map(Response::into_inner)
}

async fn uncordon(api: &JobApi, host: &str) -> Result<UncordonHostResponse, Status> {
    let request = UncordonHostRequest {
        host_id: host.to_owned(),
    };
    let answer = uncordon_host(api, Request::new(request)).await;
    answer.map(Response::into_inner)
}

fn refused<T: fmt::Debug>(answer: Result<T, Status>) -> (Code, String) {
    let status = answer.unwrap_err();
    (status.code(), status.message().to_owned())
}

fn booking(
    job: &str,
    cpu: u64,
    memory: u64,
    disk: u64,
    gpus: u32,
    ports: &[u32],
) -> wire::HostBooking {
    wire::HostBooking {
        job_id: job.to_owned(),
        cpu_millis: cpu,
        memory_bytes: memory,
        disk_bytes: disk,
        gpus,
        host_ports: ports.to_vec(),
    }
}

/// The whole shape of `host` as one booking of `job`.
fn whole(host: &wire::Host, job: &str) -> wire::HostBooking {
    let shape = host.shape.as_ref().unwrap();
    let disk = shape.disk_bytes.unwrap_or(0);
    booking(
        job,
        shape.cpu_millis,
        shape.memory_bytes,
        disk,
        shape.gpus,
        &[],
    )
}

/// Asserts that each host lists exactly what is booked on it: on `h1` a whole-host job (as
/// the host's whole shape) and an ordinary job (its share and its declared host port), in
/// the order they were booked, not in job id order; on `h2` the lease held through the lease
/// routes, under its id's text, as the whole host it books; on `h3` nothing. A releasing
/// allocation (its cancel asked, its stop not confirmed) is still listed, and once it is
/// freed it is gone. It catches bookings read from the jobs instead of the allocations, a
/// releasing allocation dropped, the lease routes' bookings missing, and bookings on the
/// wrong host.
#[tokio::test]
async fn the_host_list_shows_booked_resources() {
    let s = server("booked", true);
    s.register("h2");
    assert_eq!(s.lease(1), started("h2"));
    s.register("h1");
    s.register("h3");
    let whole_spec = JobSpec {
        needs: Needs::WholeHost,
        ..spec()
    };
    placed(&s.jobs, "job-w", "h1", whole_spec, Booked::WholeHost);
    s.store_clock.advance(Duration::from_secs(1));
    let ported = JobSpec {
        ports: vec![DeclaredPort {
            host: 8080,
            container: 80,
        }],
        ..spec()
    };
    let (job, _) = placed(&s.jobs, "job-a", "h1", ported, BOOKED);
    let hosts = list(&s.api).await;
    let ids: Vec<&str> = hosts.iter().map(|host| host.id.as_str()).collect();
    assert_eq!(ids, ["h1", "h2", "h3"]);
    let h1 = find(&hosts, "h1");
    let share = booking("job-a", 1500, 1 << 30, 2 << 30, 0, &[8080]);
    assert_eq!(h1.booked, [whole(h1, "job-w"), share.clone()]);
    let h2 = find(&hosts, "h2");
    assert_eq!(h2.booked, [whole(h2, "1-1")]);
    assert!(find(&hosts, "h3").booked.is_empty());

    s.jobs.request_cancel(&job.id).unwrap();
    s.jobs.mark_releasing(&job.id).unwrap();
    let hosts = list(&s.api).await;
    let h1 = find(&hosts, "h1");
    assert_eq!(h1.booked, [whole(h1, "job-w"), share]);
    s.jobs.free(&job.id, EndReason::Cancelled).unwrap();
    let hosts = list(&s.api).await;
    let h1 = find(&hosts, "h1");
    assert_eq!(h1.booked, [whole(h1, "job-w")]);
}

/// Asserts that a host listing that cannot read the job tables, and the answer of a cordon
/// whose host cannot then be read, are `UNAVAILABLE` `store-unavailable`, not a listing with
/// nothing booked. It catches a failed read taken as no booking.
#[tokio::test]
async fn a_listing_that_cannot_read_the_job_tables_is_unavailable() {
    let s = server("store-down", true);
    s.register("h1");
    let unavailable = (Code::Unavailable, STORE_UNAVAILABLE.to_owned());
    s.store.fail_read_after(0);
    let answer = list_hosts(&s.api, Request::new(ListHostsRequest {})).await;
    assert_eq!(refused(answer), unavailable);
    s.store.fail_read_after(0);
    assert_eq!(refused(cordon(&s.api, "h1", Some(60)).await), unavailable);
    assert_eq!(list(&s.api).await.len(), 1);
}

/// Asserts that a second cordon of the same length with the clock unmoved reports the host
/// already cordoned and leaves its expiry and the one cordon saved as they were, that a
/// shorter one does not shorten it, and that the first uncordon lifts it and the second
/// succeeds lifting nothing. It catches a repeat that stacks, extends past the request,
/// shortens, or errors.
#[tokio::test]
async fn cordon_twice_is_cordon_once() {
    let s = server("cordon-twice", true);
    s.register("h1");
    let expires = Some(T0 + 600_000);
    let first = cordon(&s.api, "h1", Some(600)).await.unwrap();
    assert!(!first.already_cordoned);
    assert_eq!(first.host.unwrap().cordon_expires_at_ms, expires);
    let second = cordon(&s.api, "h1", Some(600)).await.unwrap();
    assert!(second.already_cordoned);
    assert_eq!(second.host.unwrap().cordon_expires_at_ms, expires);
    let bound = s.registry.bindings();
    let cordoned: Vec<_> = bound
        .values()
        .filter_map(|binding| binding.cordon_until_ms())
        .collect();
    assert_eq!(cordoned, [T0 + 600_000]);
    let saved = s.bindings_file().unwrap();
    assert_eq!(saved.matches("cordon_until_ms").count(), 1, "{saved}");
    let shorter = cordon(&s.api, "h1", Some(60)).await.unwrap();
    assert!(shorter.already_cordoned);
    assert_eq!(shorter.host.unwrap().cordon_expires_at_ms, expires);

    let lifted = uncordon(&s.api, "h1").await.unwrap();
    assert!(lifted.lifted);
    assert_eq!(lifted.host.unwrap().cordon_expires_at_ms, None);
    let again = uncordon(&s.api, "h1").await.unwrap();
    assert!(!again.lifted);
    assert_eq!(again.host.unwrap().cordon_expires_at_ms, None);
    assert!(!s.bindings_file().unwrap().contains("cordon_until_ms"));
}

/// Asserts the cordon's length as the host's own route has it, through the one function
/// both use: 99999 s is cut to 3600 s, no length is 300 s, and zero is `INVALID_ARGUMENT`
/// `invalid-cordon` and places nothing. It catches a second copy of the rules that drifts.
#[tokio::test]
async fn cordon_is_clamped_and_zero_is_refused() {
    let s = server("cordon-clamp", true);
    s.register("h1");
    let cut = cordon(&s.api, "h1", Some(99_999)).await.unwrap();
    assert_eq!(cut.host.unwrap().cordon_expires_at_ms, Some(T0 + 3_600_000));
    uncordon(&s.api, "h1").await.unwrap();
    let default = cordon(&s.api, "h1", None).await.unwrap();
    assert_eq!(
        default.host.unwrap().cordon_expires_at_ms,
        Some(T0 + 300_000)
    );
    uncordon(&s.api, "h1").await.unwrap();
    let zero = cordon(&s.api, "h1", Some(0)).await;
    assert_eq!(
        refused(zero),
        (Code::InvalidArgument, INVALID_CORDON.to_owned())
    );
    let own = s.registry.own_host(&HostId::new("h1").unwrap()).unwrap();
    assert_eq!(own.cordon_until, None);
}

/// Asserts that a cordon or an uncordon of a host the registry does not hold (one that never
/// said hello, an id no host can have, or any host of a server without the agent side,
/// whose hosts are all static) is `NOT_FOUND` `unknown-host` and writes nothing. It catches
/// a static host given a cordon record, and an unknown host answered as cordoned.
#[tokio::test]
async fn cordon_of_an_unknown_or_static_host_is_not_found() {
    let unknown = (Code::NotFound, UNKNOWN_HOST.to_owned());
    let s = server("cordon-unknown", true);
    s.register("h1");
    for host in ["h9", "Not A Host!", ""] {
        assert_eq!(refused(cordon(&s.api, host, Some(60)).await), unknown);
        assert_eq!(refused(uncordon(&s.api, host).await), unknown);
    }
    assert_eq!(s.bindings_file(), None);
    let statics = server("cordon-static", false);
    assert_eq!(refused(cordon(&statics.api, "h1", Some(60)).await), unknown);
    assert_eq!(refused(uncordon(&statics.api, "h1").await), unknown);
    assert_eq!(statics.bindings_file(), None);
}

/// Asserts that a cordon placed by the call keeps a new lease off the host, and that once
/// the call lifts it the host takes one. It shows the call reaches the registry's own
/// cordon, the one placement reads, and not a copy of it.
#[tokio::test]
async fn a_cordoned_host_takes_nothing_new() {
    let s = server("cordon-placement", true);
    s.register("h1");
    cordon(&s.api, "h1", Some(600)).await.unwrap();
    assert_eq!(s.lease(1), StartAck::Refused(RefusalCode::NoCapacity));
    uncordon(&s.api, "h1").await.unwrap();
    assert_eq!(s.lease(2), started("h1"));
}

/// Asserts that a cordon and an uncordon whose bindings cannot be written are `UNAVAILABLE`
/// `cordon-unsaved`, that each holds on this server all the same, and that the same call
/// repeated once the write works is the retry. It catches an unsaved cordon acknowledged,
/// and one rolled back.
#[tokio::test]
async fn cordon_unsaved_is_unavailable_but_holds() {
    let s = server("cordon-unsaved", true);
    s.register("h1");
    let blocker = s.dir.path().join("bindings.tmp");
    std::fs::create_dir(&blocker).unwrap();
    let unsaved = (Code::Unavailable, CORDON_UNSAVED.to_owned());
    assert_eq!(refused(cordon(&s.api, "h1", Some(600)).await), unsaved);
    let id = HostId::new("h1").unwrap();
    let held = s.registry.own_host(&id).unwrap().cordon_until;
    assert_eq!(held, Some(Timestamp::from_millis(T0 + 600_000)));
    assert_eq!(s.shared.agent_errors(), 1);
    assert_eq!(refused(uncordon(&s.api, "h1").await), unsaved);
    assert_eq!(s.registry.own_host(&id).unwrap().cordon_until, None);
    std::fs::remove_dir(&blocker).unwrap();
    let retried = cordon(&s.api, "h1", Some(600)).await.unwrap();
    assert!(!retried.already_cordoned);
    assert!(s.bindings_file().unwrap().contains("cordon_until_ms"));
}

/// An ended booking of `id`, ending at `end`.
fn ended(id: &str, end: u64) -> EndedRow {
    EndedRow {
        id: AllocationId::parse(id).unwrap(),
        profile: None,
        host: None,
        booked: None,
        queued_ms: None,
        run_ms: None,
        ended_reason: EndedReason::Released,
        drain_outcome: DrainOutcome::None,
        give_up_outcome: None,
        estimated_end: None,
        actual_end: end,
        error_ms: None,
        used: None,
    }
}

/// Everything the reads could change: the registry's hosts and leases, the bindings file,
/// the job tables, the usage rows, and the counters of failed writes and of the sink.
type Snapshot = (
    Vec<HostView>,
    LeasesOn,
    Option<String>,
    Vec<(String, js_state::Versioned)>,
    Vec<(String, js_state::Versioned)>,
    (u64, u64),
    crate::usage::SinkCounters,
);

fn snapshot(s: &Server) -> Snapshot {
    (
        s.registry.hosts(),
        s.registry.held(),
        s.bindings_file(),
        s.store.scan_prefix("").unwrap(),
        s.usage.scan_prefix("").unwrap(),
        (s.shared.agent_errors(), s.shared.save_errors()),
        s.sink.counters(),
    )
}

/// Asserts that ten rounds of every read (the hosts, the pools, the providers and the
/// usage), with the clock moving between them, leave the registry, the bindings, the job
/// tables, the usage rows and every counter as they were. It catches a read that records,
/// expires, books or counts anything.
#[tokio::test]
async fn reads_change_nothing() {
    let s = server("reads", true);
    s.register("h2");
    assert_eq!(s.lease(1), started("h2"));
    s.register("h1");
    placed(&s.jobs, "job-a", "h1", spec(), BOOKED);
    cordon(&s.api, "h1", Some(600)).await.unwrap();
    for end in [10, 20, 30] {
        s.sink
            .allocation_ended(&ended(&format!("u{end}"), end))
            .unwrap();
    }
    let before = snapshot(&s);
    for _ in 0..10 {
        s.clock.advance(Duration::from_millis(1));
        s.store_clock.advance(Duration::from_millis(1));
        assert_eq!(list(&s.api).await.len(), 2);
        let pools = list_pools(&s.api, Request::new(ListPoolsRequest {})).await;
        assert!(pools.unwrap().into_inner().pools.is_empty());
        let providers = list_providers(&s.api, Request::new(ListProvidersRequest {})).await;
        assert!(providers.unwrap().into_inner().providers.is_empty());
        let usage = get_usage(&s.api, Request::new(GetUsageRequest::default())).await;
        assert_eq!(usage.unwrap().into_inner().records.len(), 3);
    }
    assert_eq!(snapshot(&s), before);
}

/// A job service whose fleet group is served over `hosts`, `pools` and `history`, over job
/// tables of its own.
fn api_over(
    hosts: Arc<dyn Hosts>,
    pools: Arc<dyn Pools>,
    history: Option<Arc<dyn History>>,
) -> JobApi {
    let store: Arc<dyn Store> = Arc::new(Fake::with_flaw(Arc::default(), Flaw::None));
    let jobs =
        Arc::new(JobStore::new(Arc::new(store), crate::job_store::ServerIds::random()).unwrap());
    let clock = SharedClock::new(ManualClock::new(Timestamp::from_millis(T0)));
    JobApi::new(jobs, clock).with_fleet(Ctx::served(hosts, pools, history))
}

/// The hosts of a fake: listed from these capacity entries, registry views, reserved ids
/// and leases, as the server lists them, and answering every cordon and uncordon with
/// `answer`.
struct FakeHosts {
    capacity: Vec<HostCapacity>,
    views: Vec<HostView>,
    reserved: Option<BTreeSet<HostId>>,
    leases: LeasesOn,
    answer: Result<bool, Refused>,
}

impl FakeHosts {
    fn new() -> FakeHosts {
        FakeHosts {
            capacity: Vec::new(),
            views: Vec::new(),
            reserved: None,
            leases: LeasesOn::new(),
            answer: Ok(true),
        }
    }
}

#[tonic::async_trait]
impl Hosts for FakeHosts {
    async fn hosts(&self) -> (HostList, LeasesOn) {
        let reserved = self.reserved.as_ref();
        let list = HostList::of(self.capacity.clone(), self.views.clone(), reserved);
        (list, self.leases.clone())
    }

    async fn cordon(&self, _: &HostId, _: Duration) -> Result<bool, Refused> {
        self.answer
    }

    async fn uncordon(&self, _: &HostId) -> Result<bool, Refused> {
        self.answer
    }
}

/// Pools from a function that builds the view.
struct FakePools(fn() -> PoolsView);

impl Pools for FakePools {
    fn pools(&self) -> PoolsView {
        (self.0)()
    }
}

fn no_pools() -> PoolsView {
    PoolsView {
        pools: Vec::new(),
        warnings: Vec::new(),
        logs: None,
        store: None,
    }
}

/// Asserts that a context is equal to its clone and to nothing else (another context over
/// other handles, the default), and that its debug text names its handles without their
/// contents. It catches a context compared by what it holds rather than by which handles.
#[test]
fn a_context_is_equal_to_its_clone_only() {
    let hosts: Arc<dyn Hosts> = Arc::new(FakeHosts::new());
    let pools: Arc<dyn Pools> = Arc::new(FakePools(no_pools));
    let served = Ctx::served(hosts, pools.clone(), None);
    assert_eq!(served.clone(), served);
    assert_ne!(served, Ctx::default());
    let other = Ctx::served(Arc::new(FakeHosts::new()), pools, None);
    assert_ne!(served, other);
    let debug = format!("{served:?}");
    assert!(debug.contains("Handle"), "{debug}");
}

/// A connector that hands the channel one end of an in-memory stream, once.
struct InMemory(Option<DuplexStream>);

impl Service<Uri> for InMemory {
    type Response = TokioIo<DuplexStream>;
    type Error = io::Error;
    type Future = Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _: Uri) -> Self::Future {
        let stream = self.0.take().map(TokioIo::new);
        ready(stream.ok_or(io::Error::from(io::ErrorKind::NotConnected)))
    }
}

/// Asserts that each of the six calls without the service's bearer token is
/// `UNAUTHENTICATED` over gRPC, the bearer layer being the only gate (no private-address
/// check is applied), and that with it the calls are served. It catches the fleet calls
/// left outside the bearer layer, or behind a gate of their own.
#[tokio::test]
async fn fleet_calls_need_the_service_token() {
    use js_proto::v1::job_service_client::JobServiceClient;
    let s = server("auth", true);
    s.register("h1");
    let token = parse_token(&[b'f'; 40]).unwrap();
    let token = crate::config::Clients::only(&token, "--token-file");
    let token = crate::config::LiveClients::new(token);
    let api = Arc::new(s.api);
    let (client_end, server_end) = tokio::io::duplex(64 * 1024);
    let service = super::super::grpc(api, token);
    let incoming = tokio_stream::once(Ok::<_, io::Error>(server_end));
    tokio::spawn(GrpcServer::builder().serve_with_incoming(service, incoming));
    let channel: Channel = Endpoint::from_static("http://in-process.invalid")
        .connect_with_connector(InMemory(Some(client_end)))
        .await
        .unwrap();
    let mut client = JobServiceClient::new(channel);
    let host = || "h1".to_owned();
    let codes = [
        client.list_hosts(ListHostsRequest {}).await.map(drop),
        client
            .cordon_host(CordonHostRequest {
                host_id: host(),
                cordon_secs: None,
            })
            .await
            .map(drop),
        client
            .uncordon_host(UncordonHostRequest { host_id: host() })
            .await
            .map(drop),
        client.list_pools(ListPoolsRequest {}).await.map(drop),
        client
            .list_providers(ListProvidersRequest {})
            .await
            .map(drop),
        client.get_usage(GetUsageRequest::default()).await.map(drop),
    ];
    for code in codes {
        assert_eq!(code.unwrap_err().code(), Code::Unauthenticated);
    }
    assert_eq!(
        s.registry
            .own_host(&HostId::new("h1").unwrap())
            .unwrap()
            .cordon_until,
        None
    );
    let mut request = tonic::Request::new(ListHostsRequest {});
    let bearer = format!("Bearer {}", "f".repeat(40));
    request
        .metadata_mut()
        .insert("authorization", bearer.parse().unwrap());
    let hosts = client.list_hosts(request).await.unwrap().into_inner().hosts;
    assert_eq!(hosts.len(), 1);
}

/// Asserts that a host listing whose read of the job tables meets `Fenced` is `UNAVAILABLE`
/// `state-fenced` (not `store-unavailable`) and trips the service's fence. It catches a fenced
/// store taken for one that may answer again, so a server with no heartbeats and nothing
/// queued keeps running without a store.
#[tokio::test]
async fn a_listing_that_meets_a_fenced_store_is_unavailable_and_trips_the_fence() {
    let s = server("store-fenced", true);
    s.register("h1");
    s.store.fail_with(js_state::StoreError::Fenced);
    s.store.fail_read_after(0);
    let answer = list_hosts(&s.api, Request::new(ListHostsRequest {})).await;
    assert_eq!(
        refused(answer),
        (Code::Unavailable, "state-fenced".to_owned())
    );
    assert!(s.api.fence.is_tripped());
}
