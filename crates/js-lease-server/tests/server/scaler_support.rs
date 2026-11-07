//! What the tests of a server with a launching pool share: a provider kind over an
//! account in memory (never a network), the text of a file for it, and the messages of a
//! launched agent.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroUsize;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use js_elastic::{
    AcquireId, AcquireRequest, Acquired, BootStep, CapacityProvider, CatalogFuture, EventStream,
    Held, HostName, HostRef, HostStatus, Offer, ProviderError, ProviderEvent, Released,
};
use js_elastic_fake::ElasticFake;
use js_lease_server::{
    BuildCtx, Built, Config, ElasticBuilt, Floor, KindError, Kinds, Launch, OwnerOnlyFiles,
    channel_ticker, start_with_checks,
};
use js_provider::StartSpec;
use js_provider_lambdalabs::{
    Architecture, ElasticConfig, FakeLambda, InstanceTypeOffer, LambdaProvider, ManualClock,
    OptionTable, Region,
};
use js_state::sqlite::{Options, SqliteStore};
use js_state::{Cond, Store, SystemClock, Txn};
use js_types::{
    Capabilities, ClassId, ExecMode, LeaseId, Model, Request, RequestId, ResourceClass, Resources,
};
use serde_json::{Value, json};

use crate::agents_support::{Session, hello_body, session};
use crate::config_support::{PoolText, file, kinds, quote, quoted, server};
use crate::store_support::sqlite_db;
use crate::support::{Harness, Kept, Reply, Scratch, request};

/// The key of the scaler's records in the state store.
pub const RECORDS_KEY: &str = "acquires/scaler/records";

/// The key of the hosts the registry refused to remove, in the state store.
pub const UNREMOVED_KEY: &str = "acquires/scaler/unremoved";

/// The accounts in memory, by the path of the key file that names them: a kind is a plain
/// function and holds nothing, so a test leaves the account here for it to find.
static ACCOUNTS: Mutex<BTreeMap<String, Arc<FakeLambda>>> = Mutex::new(BTreeMap::new());

/// The elastic fake clouds, by the path of the key file that names them, left for the same
/// kind to find (see [`elastic_cloud`]).
static CLOUDS: Mutex<BTreeMap<String, ElasticFake>> = Mutex::new(BTreeMap::new());

/// Leaves `cloud` where the `fake-cloud` kind finds it for the key file `scratch` makes: the
/// pool launches on it, and not on a Lambda account in memory.
pub fn elastic_cloud(scratch: &Scratch, cloud: &ElasticFake) {
    let key = scratch.path("api-key").display().to_string();
    CLOUDS.lock().unwrap().insert(key, cloud.clone());
}

/// The key files whose pool's provider lists late and paces its notices ([`Lagging`]), with
/// where its streams say they are back for a notice; `None` for one that sends no notices.
static LAGGING: Mutex<BTreeMap<String, Option<Sender<()>>>> = Mutex::new(BTreeMap::new());

/// Makes the provider of the pool whose key file `scratch` makes its elastic fake cloud as
/// a provider that sends no notices: a reclaim is seen in its list alone.
pub fn quiet(scratch: &Scratch) {
    let key = scratch.path("api-key").display().to_string();
    LAGGING.lock().unwrap().insert(key, None);
}

/// Makes the provider of the pool whose key file `scratch` makes a [`Lagging`] one over its
/// elastic fake cloud. The answer hears once each time the scaler's reader comes back for
/// a notice: when the stream is first read, and after each notice has been acted on.
pub fn lagging(scratch: &Scratch) -> Receiver<()> {
    let key = scratch.path("api-key").display().to_string();
    let (back, heard) = channel();
    LAGGING.lock().unwrap().insert(key, Some(back));
    heard
}

/// The elastic fake cloud whose list does not show a reclaim yet (a preempted machine is
/// listed active), and whose notice streams say when the reader comes back for the next
/// notice: the notice is then the only way the scaler learns of the reclaim. With no `back`
/// it is the cloud as it is, except that it sends no notices.
struct Lagging {
    cloud: ElasticFake,
    back: Option<Sender<()>>,
}

/// A notice stream that sends `back` a value each time it is asked for the next notice.
struct Paced {
    stream: EventStream,
    back: Sender<()>,
}

impl Iterator for Paced {
    type Item = ProviderEvent;

    fn next(&mut self) -> Option<ProviderEvent> {
        let _ = self.back.send(());
        self.stream.next()
    }
}

impl CapacityProvider for Lagging {
    fn catalog(&self) -> CatalogFuture<'_> {
        self.cloud.catalog()
    }

    fn host_prep(&self, offer: &Offer) -> Vec<BootStep> {
        self.cloud.host_prep(offer)
    }

    fn payload_limit(&self) -> usize {
        self.cloud.payload_limit()
    }

    fn acquire(&self, request: &AcquireRequest) -> Acquired {
        self.cloud.acquire(request)
    }

    fn list(&self) -> Result<Vec<Held>, ProviderError> {
        let mut held = self.cloud.list()?;
        if self.back.is_none() {
            return Ok(held);
        }
        for each in held
            .iter_mut()
            .filter(|h| h.status == HostStatus::Preempted)
        {
            each.status = HostStatus::Active;
        }
        Ok(held)
    }

    fn release(&self, id: &HostRef) -> Released {
        self.cloud.release(id)
    }

    fn unresolved(&self) -> Vec<AcquireId> {
        self.cloud.unresolved()
    }

    fn is_gone(&self, id: &HostRef) -> bool {
        self.cloud.is_gone(id)
    }

    fn id(&self) -> js_types::ProviderId {
        self.cloud.id()
    }

    fn can_launch(&self) -> bool {
        self.cloud.can_launch()
    }

    fn capabilities(&self) -> js_elastic::ProviderCaps {
        self.cloud.capabilities()
    }

    fn capacity(&self) -> js_elastic::Capacity {
        self.cloud.capacity()
    }

    fn observe(&self, id: &HostRef) -> js_elastic::Observed {
        self.cloud.observe(id)
    }

    fn verify_instance(&self, hello: &js_elastic::Hello) -> Option<HostRef> {
        self.cloud.verify_instance(hello)
    }

    fn events(&self) -> Option<EventStream> {
        let back = self.back.clone()?;
        let stream = self.cloud.events()?;
        Some(Box::new(Paced { stream, back }))
    }
}

/// The key files whose pool has a provider that panics when the scaler lists its machines.
static PANICKY: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

/// Makes the provider of the pool whose key file `scratch` makes panic at its next `list`.
pub fn panic_on_list(scratch: &Scratch) {
    let key = scratch.path("api-key").display().to_string();
    PANICKY.lock().unwrap().insert(key);
}

/// A provider that is another one in every call but `list`, which panics.
struct PanicOnList(Box<dyn CapacityProvider>);

impl CapacityProvider for PanicOnList {
    fn catalog(&self) -> CatalogFuture<'_> {
        self.0.catalog()
    }

    fn host_prep(&self, offer: &Offer) -> Vec<BootStep> {
        self.0.host_prep(offer)
    }

    fn payload_limit(&self) -> usize {
        self.0.payload_limit()
    }

    fn acquire(&self, request: &AcquireRequest) -> Acquired {
        self.0.acquire(request)
    }

    fn list(&self) -> Result<Vec<Held>, ProviderError> {
        panic!("the provider panicked in list");
    }

    fn release(&self, id: &HostRef) -> Released {
        self.0.release(id)
    }

    fn unresolved(&self) -> Vec<AcquireId> {
        self.0.unresolved()
    }

    fn is_gone(&self, id: &HostRef) -> bool {
        self.0.is_gone(id)
    }

    fn id(&self) -> js_types::ProviderId {
        self.0.id()
    }

    fn can_launch(&self) -> bool {
        self.0.can_launch()
    }

    fn capabilities(&self) -> js_elastic::ProviderCaps {
        self.0.capabilities()
    }

    fn capacity(&self) -> js_elastic::Capacity {
        self.0.capacity()
    }

    fn observe(&self, id: &HostRef) -> js_elastic::Observed {
        self.0.observe(id)
    }

    fn verify_instance(&self, hello: &js_elastic::Hello) -> Option<HostRef> {
        self.0.verify_instance(hello)
    }
}

/// The key files whose pool's provider reads the store before each launch ([`Peek`]).
static PEEKS: Mutex<BTreeMap<String, Arc<Peek>>> = Mutex::new(BTreeMap::new());

/// What the provider of a pool saw of the scaler's records in the state store at each launch,
/// read through a handle of its own the moment the scaler called it: the value of
/// [`RECORDS_KEY`], or `None` when the store held none.
#[derive(Default)]
pub struct Peek {
    store: OnceLock<Arc<dyn Store>>,
    seen: Mutex<Vec<Option<String>>>,
}

impl Peek {
    /// Reads `store` at every launch from now on.
    pub fn watch(&self, store: Arc<dyn Store>) {
        assert!(self.store.set(store).is_ok(), "one store per peek");
    }

    /// What each launch saw, in order.
    pub fn seen(&self) -> Vec<Option<String>> {
        self.seen.lock().unwrap().clone()
    }

    fn look(&self) {
        let store = self.store.get().expect("the test gave the store");
        let row = store.get(RECORDS_KEY).unwrap();
        let text = row.map(|row| String::from_utf8(row.value).unwrap());
        self.seen.lock().unwrap().push(text);
    }
}

/// Makes the provider of the pool whose key file `scratch` makes read the store before each
/// launch, as the returned [`Peek`] says.
pub fn peek(scratch: &Scratch) -> Arc<Peek> {
    let key = scratch.path("api-key").display().to_string();
    let peek = Arc::new(Peek::default());
    PEEKS.lock().unwrap().insert(key, Arc::clone(&peek));
    peek
}

/// A provider that is another one in every call, and reads the store before a launch.
struct Peeking(Box<dyn CapacityProvider>, Arc<Peek>);

impl CapacityProvider for Peeking {
    fn catalog(&self) -> CatalogFuture<'_> {
        self.0.catalog()
    }

    fn checks(&self) -> Vec<js_elastic::CheckResult> {
        self.0.checks()
    }

    fn host_prep(&self, offer: &Offer) -> Vec<BootStep> {
        self.0.host_prep(offer)
    }

    fn payload_limit(&self) -> usize {
        self.0.payload_limit()
    }

    fn acquire(&self, request: &AcquireRequest) -> Acquired {
        self.1.look();
        self.0.acquire(request)
    }

    fn list(&self) -> Result<Vec<Held>, ProviderError> {
        self.0.list()
    }

    fn release(&self, id: &HostRef) -> Released {
        self.0.release(id)
    }

    fn unresolved(&self) -> Vec<AcquireId> {
        self.0.unresolved()
    }

    fn is_gone(&self, id: &HostRef) -> bool {
        self.0.is_gone(id)
    }

    fn id(&self) -> js_types::ProviderId {
        self.0.id()
    }

    fn can_launch(&self) -> bool {
        self.0.can_launch()
    }

    fn capabilities(&self) -> js_elastic::ProviderCaps {
        self.0.capabilities()
    }

    fn capacity(&self) -> js_elastic::Capacity {
        self.0.capacity()
    }

    fn observe(&self, id: &HostRef) -> js_elastic::Observed {
        self.0.observe(id)
    }

    fn verify_instance(&self, hello: &js_elastic::Hello) -> Option<HostRef> {
        self.0.verify_instance(hello)
    }

    fn events(&self) -> Option<EventStream> {
        self.0.events()
    }
}

/// The one instance type of the fake account.
const OPTIONS: &str = r#"
[[option]]
instance_type = "gpu_1x_a100"
"#;

/// The agent the bootstrap downloads.
pub const AGENT_URL: &str = "https://sched.example.org:7301/agent";
pub const AGENT_SHA256: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

/// The `fake-cloud` kind: a Lambda provider over the account in memory named by the key
/// file path. It is enabled and live exactly when the pool's `launch` is, as the real kind
/// is. Its floor is one GPU of class `nvidia-a100`.
fn fake_cloud(table: &toml::Table, ctx: &BuildCtx<'_>) -> Result<Built, KindError> {
    let path = table.get("api_key_file").and_then(toml::Value::as_str);
    let elastic = CLOUDS
        .lock()
        .unwrap()
        .get(path.unwrap_or_default())
        .cloned();
    if let Some(cloud) = elastic {
        let lagging = LAGGING
            .lock()
            .unwrap()
            .get(path.unwrap_or_default())
            .cloned();
        let provider: Box<dyn CapacityProvider> = match lagging {
            Some(back) => Box::new(Lagging { cloud, back }),
            None => Box::new(cloud),
        };
        return Ok(Built::Elastic(ElasticBuilt {
            idle_costs: false,
            provider,
            floor: Floor {
                gpus: 1,
                gpu_class: Some(Model::parse("nvidia-a100").unwrap()),
            },
            exec: vec![ExecMode::Container],
        }));
    }
    let account = ACCOUNTS
        .lock()
        .unwrap()
        .get(path.unwrap_or_default())
        .cloned();
    let Some(account) = account else {
        return Err(KindError {
            field: "api_key_file".to_owned(),
            reason: "no account in memory".to_owned(),
        });
    };
    let size = NonZeroUsize::new(4).unwrap();
    let config = ElasticConfig::new(ctx.scheduler.clone(), "test-key", size).unwrap();
    let config = config.launch_interval(Duration::ZERO);
    let config = match ctx.launch {
        Launch::Live => config.enable().live(),
        Launch::Plan => config,
    };
    let options = OptionTable::parse(OPTIONS).unwrap();
    let clock = Arc::new(ManualClock::new());
    let provider = LambdaProvider::new(account, clock, options, config);
    let panicky = PANICKY.lock().unwrap().contains(path.unwrap_or_default());
    let provider: Box<dyn CapacityProvider> = match panicky {
        true => Box::new(PanicOnList(Box::new(provider))),
        false => Box::new(provider),
    };
    let peek = PEEKS.lock().unwrap().get(path.unwrap_or_default()).cloned();
    let provider: Box<dyn CapacityProvider> = match peek {
        Some(peek) => Box::new(Peeking(provider, peek)),
        None => provider,
    };
    Ok(Built::Elastic(ElasticBuilt {
        provider,
        floor: Floor {
            gpus: 1,
            gpu_class: Some(Model::parse("nvidia-a100").unwrap()),
        },
        exec: vec![ExecMode::Container],
        idle_costs: false,
    }))
}

/// The kinds of these tests: the configuration tests' and `fake-cloud`.
pub fn cloud_kinds() -> Kinds {
    kinds().with("fake-cloud", fake_cloud)
}

/// A fake account that offers `gpu_1x_a100` in one region, left where `fake-cloud` finds it
/// for the key file `scratch` makes.
pub fn account(scratch: &Scratch) -> Arc<FakeLambda> {
    let account = Arc::new(FakeLambda::new());
    // The key the pool names (`ElasticConfig::new` in `fake_cloud`): the provider's own
    // checks find it on the account.
    account.add_ssh_key("test-key");
    account.set_offers(vec![InstanceTypeOffer {
        name: "gpu_1x_a100".to_owned(),
        description: String::new(),
        gpu_description: String::new(),
        price_cents_per_hour: 129,
        vcpus: 8,
        memory_gib: 64,
        storage_gib: 512,
        gpus: 1,
        architecture: Architecture::X86_64,
        regions: vec![Region {
            name: "us-west-1".to_owned(),
            description: String::new(),
        }],
    }]);
    let key = scratch.path("api-key").display().to_string();
    ACCOUNTS.lock().unwrap().insert(key, account.clone());
    account
}

/// The text of a file with one pool `burst` of the fake cloud, in `launch` mode, whose
/// hosts are idle after a minute. `boot` is `initialise` or `resume`.
pub fn text(scratch: &Scratch, launch: &str, boot: &str) -> String {
    text_with(scratch, launch, boot, &[])
}

/// [`text`] with these keys of the pool set to these TOML values; an empty value leaves the
/// key out.
pub fn text_with(scratch: &Scratch, launch: &str, boot: &str, more: &[(&str, &str)]) -> String {
    let key = scratch.path("api-key");
    let provider = format!(
        "{{ kind = \"fake-cloud\", api_key_file = {} }}",
        quoted(&key)
    );
    let agent = format!(
        "{{ x86_64 = {{ url = {}, sha256 = {} }} }}",
        quote(AGENT_URL),
        quote(AGENT_SHA256)
    );
    let mut pool = PoolText::elastic(scratch)
        .set("name", &quote("burst"))
        .set("exec", &quote("none"))
        .set("idle", &quote("1m"))
        .set("min_host_lifetime", &quote("1m"))
        .set("launch", &quote(launch))
        .set("agent", &agent)
        .set("provider", &provider);
    for (key, value) in more {
        pool = match value.is_empty() {
            true => pool.without(key),
            false => pool.set(key, value),
        };
    }
    let extra = "listen = \"127.0.0.1:0\"\nagent_listen = \"127.0.0.2:0\"\n\
                 advertise_url = \"https://sched.example.org:7301\"";
    let head = server(scratch, extra).replace("\"initialise\"", &quote(boot));
    file(&head, &[&pool])
}

/// [`text`], with the `[server]` keys of `kept` ([`Kept::keys`]): with the store, the state is
/// kept in a SQLite store of `scratch`.
pub fn text_in(scratch: &Scratch, launch: &str, boot: &str, kept: Kept) -> String {
    let keys = format!("[server]\n{}", kept.keys(scratch));
    text(scratch, launch, boot).replacen("[server]\n", &keys, 1)
}

/// A server on `text`, read with the fake cloud's kinds, whose pool has had its first
/// check pass: the scaler launches only while the checks pass.
pub async fn start(scratch: Scratch, text: &str) -> Harness {
    let path = scratch.write("js.toml", text, 0o644);
    let clock = crate::config_support::clock();
    let config = Config::from_config_file(&path, &cloud_kinds(), &OwnerOnlyFiles, clock);
    let harness = Harness::over(scratch, config.unwrap()).await;
    harness.checks.step().await;
    harness
}

/// Starts a server on `text` that must be refused at start, and says why. The tickers are
/// not stepped.
pub async fn start_refused(scratch: &Scratch, text: &str) -> String {
    let path = scratch.write("js.toml", text, 0o644);
    let clock = crate::config_support::clock();
    let config = Config::from_config_file(&path, &cloud_kinds(), &OwnerOnlyFiles, clock.clone());
    let (_ticks, ticker) = channel_ticker();
    let (_checks, check_ticker) = channel_ticker();
    let refused = start_with_checks(config.unwrap(), clock, ticker, check_ticker).await;
    refused.err().unwrap().to_string()
}

/// The start of a whole-node request `id` for one a100, under the lease `lease`.
pub fn spec(lease: u64, id: &str) -> StartSpec {
    let request = Request {
        class: ClassId::new("whole-node").unwrap(),
        resources: Resources::new(1000, 1024, 1, Some("nvidia-a100".to_owned())).unwrap(),
        capabilities: Capabilities::new(),
        max_wall: Duration::from_secs(7200),
        labels: BTreeMap::new(),
        validation_run: None,
        probe_host: None,
    };
    let spec = StartSpec::new(
        LeaseId::new(1, lease),
        request,
        &[ResourceClass::whole_node()],
    );
    spec.unwrap().with_request_id(RequestId::new(id).unwrap())
}

/// The join token the user data of a launch writes for the agent.
pub fn token_in(script: &str) -> String {
    let head = "printf '%s' '";
    let line = script
        .lines()
        .find(|line| line.starts_with(head) && line.ends_with("/etc/jobscheduler/token"))
        .expect("the script writes the token");
    line[head.len()..].split("' > ").next().unwrap().to_owned()
}

/// The id the agent of a launch registers under, as the unit in the user data names it
/// (`--host-id "<id>"`).
pub fn host_id_in(script: &str) -> String {
    let head = "--host-id \"";
    let at = script.find(head).expect("the unit names the host id") + head.len();
    script[at..].split('"').next().unwrap().to_owned()
}

/// The name `scheduler` gives the machine of `acquire`.
pub fn name_of(scheduler: &str, acquire: &str) -> String {
    let scheduler = js_elastic::SchedulerId::new(scheduler).unwrap();
    let acquire = js_elastic::AcquireId::new(acquire).unwrap();
    HostName::derive(&scheduler, &acquire).to_string()
}

/// The hello of the launched machine `name`, which names the instance it runs on and
/// announces the size of the offer it was launched from (8 cores, 64 GiB, one a100).
pub fn hello_body_of(name: &str) -> Value {
    let mut body = hello_body(name);
    body["instance"] = json!({ "provider": "burst", "instance_id": name });
    body["shape"]["memory_bytes"] = json!(64u64 << 30);
    // The class the agent of a `gpu_1x_a100` machine reads from `nvidia-smi`: the 40 GB
    // PCIe variant the provider's catalog table offers for that type.
    body["shape"]["gpu_class"] = json!("nvidia-a100-pcie-40gb");
    body
}

/// A hello of `name` with `token`, on the agent port.
pub async fn hello(h: &Harness, token: &str, name: &str) -> Reply {
    let bytes = hello_body_of(name).to_string().into_bytes();
    let addr = h.running.agent_addr().unwrap();
    request(addr, "POST", "/v1/agent/hello", Some(token), Some(&bytes)).await
}

/// A hello of `name` that must be admitted: the session it opened.
pub async fn join(h: &Harness, token: &str, name: &str) -> Session {
    session(&hello(h, token, name).await, name)
}

/// A heartbeat of `session` with `token`, on the agent port.
pub async fn beat(h: &Harness, token: &str, session: &Session) -> Reply {
    let bytes = session.beat().to_string().into_bytes();
    let addr = h.running.agent_addr().unwrap();
    request(
        addr,
        "POST",
        "/v1/agent/heartbeat",
        Some(token),
        Some(&bytes),
    )
    .await
}

/// The path of the scaler's records beside the lease file of `scratch`.
pub fn records_path(scratch: &Scratch) -> std::path::PathBuf {
    let mut name = scratch.state().file_name().unwrap().to_os_string();
    name.push(".scaler");
    scratch.state().with_file_name(name)
}

/// The path of the hosts the registry refused to remove, beside the lease file of `scratch`.
pub fn unremoved_path(scratch: &Scratch) -> std::path::PathBuf {
    let mut name = records_path(scratch).into_os_string();
    name.push(".unremoved");
    std::path::PathBuf::from(name)
}

/// What `h` keeps at `key` of the state store, or, without a store, in the file at `path`:
/// none when there is neither.
fn kept_at(h: &Harness, path: std::path::PathBuf, key: &str) -> Option<String> {
    match h.running.store() {
        None => std::fs::read_to_string(path).ok(),
        Some(store) => {
            let row = store.get(key).unwrap();
            row.map(|row| String::from_utf8(row.value).unwrap())
        }
    }
}

/// The text of the scaler's records `h` keeps: the file beside the lease file, or the value
/// of [`RECORDS_KEY`].
pub fn kept_records(h: &Harness) -> Option<String> {
    kept_at(h, records_path(&h.scratch), RECORDS_KEY)
}

/// The text of the hosts to remove `h` keeps: the file beside the lease file, or the value of
/// [`UNREMOVED_KEY`].
pub fn kept_unremoved(h: &Harness) -> Option<String> {
    kept_at(h, unremoved_path(&h.scratch), UNREMOVED_KEY)
}

/// The SQLite store of `scratch`, opened by the test while no server holds it.
pub fn open_store(scratch: &Scratch) -> SqliteStore {
    let clock = Arc::new(SystemClock::default());
    SqliteStore::open(&sqlite_db(scratch), clock, Options::default()).unwrap()
}

/// Puts `text` at `key` of the SQLite store of `scratch`, which no server holds.
pub fn put_in_store(scratch: &Scratch, key: &str, text: &str) {
    let txn = Txn::new().put(key, text, Cond::Any);
    open_store(scratch).txn(txn).unwrap().unwrap();
}
