//! What the tests of a server with a launching pool share: a provider kind over an
//! account in memory (never a network), the text of a file for it, and the messages of a
//! launched agent.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use js_elastic::{
    AcquireId, AcquireRequest, Acquired, BootStep, ElasticProvider, GpuClass, Held, HostName,
    HostRef, Offer, ProviderError, Released,
};
use js_lease_server::{
    BuildCtx, Built, Config, ElasticBuilt, Floor, KindError, Kinds, Launch, OwnerOnlyFiles,
    channel_ticker, start_with_checks,
};
use js_provider::StartSpec;
use js_provider_lambdalabs::{
    Architecture, ElasticConfig, FakeLambda, InstanceTypeOffer, LambdaProvider, ManualClock,
    OptionTable, Region,
};
use js_types::{
    Capabilities, ClassId, ExecMode, LeaseId, Request, RequestId, ResourceClass, Resources,
};
use serde_json::{Value, json};

use crate::agents_support::{Session, hello_body, session};
use crate::config_support::{PoolText, file, kinds, quote, quoted, server};
use crate::support::{Harness, Reply, Scratch, request};

/// The accounts in memory, by the path of the key file that names them: a kind is a plain
/// function and holds nothing, so a test leaves the account here for it to find.
static ACCOUNTS: Mutex<BTreeMap<String, Arc<FakeLambda>>> = Mutex::new(BTreeMap::new());

/// The key files whose pool has a provider that panics when the scaler lists its machines.
static PANICKY: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

/// Makes the provider of the pool whose key file `scratch` makes panic at its next `list`.
pub fn panic_on_list(scratch: &Scratch) {
    let key = scratch.path("api-key").display().to_string();
    PANICKY.lock().unwrap().insert(key);
}

/// A provider that is another one in every call but `list`, which panics.
struct PanicOnList(Box<dyn ElasticProvider>);

impl ElasticProvider for PanicOnList {
    fn catalog(&self) -> Result<Vec<Offer>, ProviderError> {
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
}

/// The one instance type of the fake account.
const OPTIONS: &str = r#"
[[option]]
instance_type = "gpu_1x_a100"
gpu_class = "nvidia-a100"
gpus = 1
"#;

/// The agent the bootstrap downloads.
pub const AGENT_URL: &str = "https://sched.example.org:7301/agent";
pub const AGENT_SHA256: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

/// The `fake-cloud` kind: a Lambda provider over the account in memory named by the key
/// file path. It is enabled and live exactly when the pool's `launch` is, as the real kind
/// is. Its floor is one GPU of class `nvidia-a100`.
fn fake_cloud(table: &toml::Table, ctx: &BuildCtx<'_>) -> Result<Built, KindError> {
    let path = table.get("api_key_file").and_then(toml::Value::as_str);
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
    let provider: Box<dyn ElasticProvider> = match panicky {
        true => Box::new(PanicOnList(Box::new(provider))),
        false => Box::new(provider),
    };
    Ok(Built::Elastic(ElasticBuilt {
        provider,
        floor: Floor {
            gpus: 1,
            gpu_class: Some(GpuClass::parse("nvidia-a100").unwrap()),
        },
        exec: vec![ExecMode::Container],
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
