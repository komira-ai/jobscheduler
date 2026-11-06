//! The order of one in-process tick over an elastic pool on the elastic fake cloud: the
//! pool's checks, then the lease table, then the scaler, read from one log the cloud and the
//! static pool both write to.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use js_elastic::{
    AcquireId, AcquireRequest, Acquired, BootStep, CapacityProvider, CatalogFuture, CheckResult,
    Held, HostRef, Offer, PriceUnit, ProviderError, Released,
};
use js_elastic_fake::{Billing, ElasticFake, FakeClock};
use js_jobs::{Constraints, JobId, JobSpec, JobStore, Needs, NetworkPolicy, NewJob, RandomIds};
use js_lease::Booking;
use js_lease_server::in_process::InProcess;
use js_lease_server::{
    BuildCtx, Built, CapacitySource, Config, ElasticBuilt, Floor, KindError, Kinds, OwnerOnlyFiles,
    Seated, StartupError,
};
use js_pool_static::{EXAMPLE_CONFIG, StaticPool};
use js_provider::{HostCapacity, Observed, Pool, StartAck, StartSpec, StopAck};
use js_registry::{SequentialSessions, SessionSource};
use js_state::sqlite::{Options, SqliteStore};
use js_types::{
    ExecMode, ImageDigestRef, Labels, LeaseId, Model, ProviderId, ResourceClass, Resources,
    accelerator_of,
};

use crate::support::{Scratch, Stepping, TOKEN, clock, lease_body, routed};

/// What the cloud's provider and the static pool were asked, in order.
type Log = Arc<Mutex<Vec<String>>>;

/// The clouds and their logs, by the path of the key file that names them: a kind is a
/// plain function and holds nothing, so the test leaves its cloud here for it to find.
static CLOUDS: Mutex<BTreeMap<String, (ElasticFake, Log)>> = Mutex::new(BTreeMap::new());

/// The elastic fake cloud, writing its pool checks' calls (`checks`), and the scaler's
/// (`list`, `acquire`) to the log.
struct Logged {
    cloud: ElasticFake,
    log: Log,
}

impl Logged {
    fn note(&self, call: &str) {
        self.log.lock().unwrap().push(call.to_owned());
    }
}

impl CapacityProvider for Logged {
    fn id(&self) -> ProviderId {
        self.cloud.id()
    }

    fn can_launch(&self) -> bool {
        self.cloud.can_launch()
    }

    fn capabilities(&self) -> js_elastic::ProviderCaps {
        self.cloud.capabilities()
    }

    fn catalog(&self) -> CatalogFuture<'_> {
        self.cloud.catalog()
    }

    fn capacity(&self) -> js_elastic::Capacity {
        self.cloud.capacity()
    }

    fn checks(&self) -> Vec<CheckResult> {
        self.note("checks");
        self.cloud.checks()
    }

    fn host_prep(&self, offer: &Offer) -> Vec<BootStep> {
        self.cloud.host_prep(offer)
    }

    fn payload_limit(&self) -> usize {
        self.cloud.payload_limit()
    }

    fn acquire(&self, request: &AcquireRequest) -> Acquired {
        self.note("acquire");
        self.cloud.acquire(request)
    }

    fn list(&self) -> Result<Vec<Held>, ProviderError> {
        self.note("list");
        self.cloud.list()
    }

    fn release(&self, id: &HostRef) -> Released {
        self.cloud.release(id)
    }

    fn unresolved(&self) -> Vec<AcquireId> {
        self.cloud.unresolved()
    }

    fn observe(&self, id: &HostRef) -> js_elastic::Observed {
        self.cloud.observe(id)
    }

    fn is_gone(&self, id: &HostRef) -> bool {
        self.cloud.is_gone(id)
    }

    fn verify_instance(&self, hello: &js_elastic::Hello) -> Option<HostRef> {
        self.cloud.verify_instance(hello)
    }

    fn events(&self) -> Option<js_elastic::EventStream> {
        self.cloud.events()
    }
}

/// The `logged-cloud` kind: the cloud left in [`CLOUDS`] for the pool's key file, behind
/// [`Logged`]. Its floor is one GPU of class `nvidia-a100`, on hosts that run containers.
fn logged_cloud(table: &toml::Table, _ctx: &BuildCtx<'_>) -> Result<Built, KindError> {
    let path = table.get("api_key_file").and_then(toml::Value::as_str);
    let clouds = CLOUDS.lock().unwrap();
    let (cloud, log) = clouds.get(path.unwrap()).cloned().unwrap();
    Ok(Built::Elastic(ElasticBuilt {
        provider: Box::new(Logged { cloud, log }),
        floor: Floor {
            gpus: 1,
            gpu_class: Some(Model::parse("nvidia-a100").unwrap()),
        },
        exec: vec![ExecMode::Container],
    }))
}

/// The shipped two-host static pool, writing the table's stops (`stop <lease>`) to the log.
struct LoggedStatic(Log);

struct LoggedPool {
    pool: StaticPool,
    log: Log,
}

impl CapacitySource for LoggedStatic {
    fn provider(&self, _bookings: &[Booking]) -> Result<Seated, StartupError> {
        let id = ProviderId::new("static").unwrap();
        let pool = StaticPool::from_toml(id, EXAMPLE_CONFIG).unwrap();
        let log = self.0.clone();
        Ok(Seated::all(Box::new(LoggedPool { pool, log })))
    }
}

impl Pool for LoggedPool {
    fn id(&self) -> ProviderId {
        self.pool.id()
    }

    fn classes(&self) -> Vec<ResourceClass> {
        self.pool.classes()
    }

    fn start(&mut self, spec: StartSpec) -> StartAck {
        self.pool.start(spec)
    }

    fn stop(&mut self, lease: &LeaseId) -> StopAck {
        self.log.lock().unwrap().push(format!("stop {lease}"));
        self.pool.stop(lease)
    }

    fn observe(&mut self, lease: &LeaseId) -> Observed {
        self.pool.observe(lease)
    }

    fn capacity(&self) -> Vec<HostCapacity> {
        self.pool.capacity()
    }
}

/// The one offer of the cloud: one a100 on 8 cores and 64 GiB.
fn offer() -> Offer {
    Offer {
        id: "gpu_1x_a100".to_owned(),
        zone: Some("us-west-1".to_owned()),
        cpu_millis: 8000,
        memory_bytes: 64 << 30,
        disk_bytes: 512 << 30,
        accelerator: Some(accelerator_of(
            &Model::parse("nvidia-a100").unwrap(),
            1,
            None,
        )),
        os: "linux".to_owned(),
        arch: "x86_64".to_owned(),
        isa_level: None,
        exec: ExecMode::Container,
        price_cents_per_hour: Some(120),
        price_unit: PriceUnit::UsCents,
        available: true,
        purchase: None,
        quota: None,
        block: None,
    }
}

/// The configuration file of a server with one live pool `burst` of the `logged-cloud` kind,
/// leases of 60 s, the agent routes on a port of their own (which an elastic pool needs,
/// and which the seam does not bind), and the job service on, so the server holds the job
/// tables beside the state file (the seam binds no job listener either).
fn file_text(scratch: &Scratch) -> String {
    let token = scratch.write("client-token", TOKEN, 0o600);
    let key = scratch.write("api-key", "test-api-key", 0o600);
    let quoted = |path: &std::path::Path| format!("{:?}", path.display().to_string());
    let sha = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
    format!(
        "[server]\n\
         state_file = {state}\n\
         boot = \"initialise\"\n\
         client_token_file = {token}\n\
         max_lease_wall = \"2h\"\n\
         default_ttl = \"60s\"\n\
         listen = \"127.0.0.1:0\"\n\
         agent_listen = \"127.0.0.2:0\"\n\
         job_listen = \"127.0.0.3:0\"\n\
         advertise_url = \"https://sched.example.org:7301\"\n\
         \n\
         [[pool]]\n\
         name = \"burst\"\n\
         class = \"whole-node\"\n\
         max_instances = 4\n\
         max_spend_per_hour = \"12.00\"\n\
         max_spend_per_day = \"96.00\"\n\
         idle = \"1m\"\n\
         max_age = \"2h\"\n\
         min_host_lifetime = \"1m\"\n\
         exec = \"container\"\n\
         launch = \"live\"\n\
         agent = {{ x86_64 = {{ url = \"https://sched.example.org:7301/agent\", sha256 = \"{sha}\" }} }}\n\
         provider = {{ kind = \"logged-cloud\", api_key_file = {key} }}\n",
        state = quoted(&scratch.state()),
        token = quoted(&token),
        key = quoted(&key),
    )
}

/// Writes one queued job, `id`, asking for one core, 1 GiB and one a100, into the job tables
/// beside the state file of `scratch`, before the server opens them: the demand the scaler
/// reads.
fn queue_job(scratch: &Scratch, id: &str) {
    let mut path = scratch.state().into_os_string();
    path.push(".jobs");
    let clock = Arc::new(js_state::ManualClock::default());
    clock.advance(Duration::from_millis(1_000_000_000_000));
    let store = SqliteStore::open(std::path::Path::new(&path), clock, Options::default()).unwrap();
    let store: Arc<dyn js_state::Store> = Arc::new(store);
    let jobs = JobStore::new(Arc::new(store), RandomIds).unwrap();
    let spec = JobSpec {
        image: ImageDigestRef::parse(&format!("team/app@sha256:{}", "a".repeat(64))).unwrap(),
        command: vec!["run".to_owned()],
        args: Vec::new(),
        env: BTreeMap::new(),
        needs: Needs::Shape(
            Resources::new(1000, 1 << 30, 1, Some("nvidia-a100".to_owned())).unwrap(),
        ),
        constraints: Constraints::default(),
        wall_limit_secs: 600,
        network: NetworkPolicy::None,
        ports: Vec::new(),
        files: js_jobs::JobFiles::default(),
        labels: Labels::default(),
    };
    let new = NewJob {
        id: Some(JobId::parse(id).unwrap()),
        spec,
        priority: 0,
        client: None,
    };
    jobs.insert_queued(new).unwrap();
}

/// Where `call` first appears in `calls`.
fn first(calls: &[String], call: &str) -> usize {
    calls
        .iter()
        .position(|each| each == call)
        .unwrap_or_else(|| panic!("no {call} in {calls:?}"))
}

/// Asserts the order of a tick over an elastic pool with a job queued from the start. The
/// first tick runs the pool's first pass of checks (so the pool is available) and then the
/// scaler, which launches for the job: the log shows the checks before the first `acquire`.
/// Then a lease on the static pool passes its ttl and the checks fall due again (60 s); in
/// the tick that follows, the table's stop of the expired lease comes before the scaler's
/// first call (its `list`), and the job, still on its way to its machine, launches nothing
/// more. It catches a tick that runs the scaler before the checks (the scaler would read the
/// pool as still being checked and launch nothing), one that skips the checks (no `checks`,
/// and no launch: the scaler launches only while they pass), and one that runs the scaler
/// before the table.
#[tokio::test]
async fn one_tick_runs_the_checks_then_the_table_then_the_scaler() {
    let scratch = Scratch::new("order");
    let fake_clock = FakeClock::new(1_000_000_000_000);
    let cloud = ElasticFake::new(fake_clock.clone(), Billing::Lambda, vec![offer()]);
    let log: Log = Arc::default();
    let key = scratch.path("api-key").display().to_string();
    CLOUDS.lock().unwrap().insert(key, (cloud, log.clone()));
    let path = scratch.write("js.toml", &file_text(&scratch), 0o644);
    queue_job(&scratch, "wants-a100");
    let (clock, shared) = clock();
    let kinds = Kinds::standard().with("logged-cloud", logged_cloud);
    let config = Config::from_config_file(&path, &kinds, &OwnerOnlyFiles, shared.clone()).unwrap();
    let config = config
        .with_capacity_source(Box::new(LoggedStatic(log.clone())))
        .with_monotonic(Arc::new(Stepping::of(Duration::from_millis(1))));
    let server = InProcess::start(config, shared).unwrap();
    let client = server.client_router();
    let granted = routed(
        &client,
        "POST",
        "/v1/leases",
        Some(TOKEN),
        Some(&lease_body()),
    )
    .await;
    assert_eq!(granted.status, 201, "{:?}", granted.body);

    let before = log.lock().unwrap().len();
    server.tick().await;
    let calls = log.lock().unwrap()[before..].to_vec();
    assert!(first(&calls, "checks") < first(&calls, "list"), "{calls:?}");
    assert!(
        first(&calls, "checks") < first(&calls, "acquire"),
        "{calls:?}"
    );

    clock.advance(Duration::from_secs(61));
    fake_clock.advance(Duration::from_secs(61));
    let before = log.lock().unwrap().len();
    server.tick().await;
    let calls = log.lock().unwrap()[before..].to_vec();
    assert!(
        first(&calls, "stop 1-1") < first(&calls, "list"),
        "{calls:?}"
    );
    assert!(!calls.iter().any(|call| call == "acquire"), "{calls:?}");
    let scaler = server.scaler().unwrap();
    assert_eq!(scaler.state().records.instances.len(), 1);
}

/// Starts a server with the live pool `burst` of [`file_text`] on a fresh elastic fake cloud
/// and one job queued from the start, its scaler drawing from `entropy` when one is given,
/// else from the default the configuration has, and runs one tick: the pool's checks pass
/// and the scaler launches one machine for the job with its first draw. Returns the acquire
/// id and the name of each machine the scaler then has a record of.
async fn launches(name: &str, entropy: Option<Arc<dyn SessionSource>>) -> Vec<(String, String)> {
    let scratch = Scratch::new(name);
    let fake_clock = FakeClock::new(1_000_000_000_000);
    let cloud = ElasticFake::new(fake_clock, Billing::Lambda, vec![offer()]);
    let log: Log = Arc::default();
    let key = scratch.path("api-key").display().to_string();
    CLOUDS.lock().unwrap().insert(key, (cloud, log.clone()));
    let path = scratch.write("js.toml", &file_text(&scratch), 0o644);
    queue_job(&scratch, "wants-a100");
    let (_clock, shared) = clock();
    let kinds = Kinds::standard().with("logged-cloud", logged_cloud);
    let config = Config::from_config_file(&path, &kinds, &OwnerOnlyFiles, shared.clone()).unwrap();
    let config = config
        .with_capacity_source(Box::new(LoggedStatic(log)))
        .with_monotonic(Arc::new(Stepping::of(Duration::from_millis(1))));
    let config = match entropy {
        Some(entropy) => config.with_entropy(entropy),
        None => config,
    };
    let server = InProcess::start(config, shared).unwrap();
    server.tick().await;
    let records = server.scaler().unwrap().state().records.instances;
    let named = records.iter().map(|record| {
        let acquire = record.acquire.as_ref().map(ToString::to_string);
        (acquire.unwrap_or_default(), record.name.clone())
    });
    named.collect()
}

/// Asserts that two servers whose scalers are given seeded sources that answer the same
/// bytes ([`SequentialSessions`], each its own) launch the same machine, with the same
/// acquire id and the same host name; that the id is the pool's name and the source's first
/// draw in hex; and that a source shared by one server and a server started after it (the
/// same `Arc`) goes on with the next draw, so the second names a machine the first did not.
/// It catches a scaler that ignores `Config::with_entropy` (its id would be the operating
/// system's random one, different on each run), one that draws other than once a step, and
/// a copy of the source that starts it over.
#[tokio::test]
async fn seeded_entropy_names_the_same_machines_on_every_run() {
    let first_run = launches("seeded-a", Some(Arc::new(SequentialSessions::new()))).await;
    let second_run = launches("seeded-b", Some(Arc::new(SequentialSessions::new()))).await;
    assert_eq!(first_run.len(), 1, "{first_run:?}");
    assert_eq!(first_run, second_run);
    let (id, name) = &first_run[0];
    assert_eq!(*id, format!("burst-{:032x}", 1));
    assert!(name.starts_with("js-"), "{name}");

    let shared: Arc<dyn SessionSource> = Arc::new(SequentialSessions::new());
    let before = launches("shared-a", Some(Arc::clone(&shared))).await;
    let after = launches("shared-b", Some(shared)).await;
    assert_eq!(before, first_run);
    assert_eq!(after.len(), 1, "{after:?}");
    assert_eq!(after[0].0, format!("burst-{:032x}", 2));
    assert_ne!(after[0].1, before[0].1);
}

/// Asserts that a server given no source keeps the production default, the operating
/// system's random source: the id is the pool's name and 32 hex digits whose value is past
/// any small sequential count, and two runs on the same inputs name different machines.
/// The chance two draws of 128 random bits agree is 2^-128, so the test does not flicker.
/// It catches a default that is a fixed or sequential source, which would name the same
/// machines on two production servers.
#[tokio::test]
async fn without_a_source_the_scaler_draws_from_the_operating_system() {
    let first_run = launches("default-a", None).await;
    let second_run = launches("default-b", None).await;
    assert_eq!(first_run.len(), 1, "{first_run:?}");
    assert_eq!(second_run.len(), 1, "{second_run:?}");
    for (id, _) in first_run.iter().chain(&second_run) {
        let hex = id.strip_prefix("burst-").unwrap_or_else(|| panic!("{id}"));
        assert_eq!(hex.len(), 32, "{id}");
        let value = u128::from_str_radix(hex, 16).unwrap();
        assert!(value > u128::from(u64::MAX), "{id}");
    }
    assert_ne!(first_run, second_run);
}
