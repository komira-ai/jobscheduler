//! A provider over the in-memory account, with a clock that moves only when told to.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use js_elastic::{
    AcquireId, AcquireRequest, BootPayload, CapacityProvider, HostName, HostRef, HostStatus, Offer,
    PriceUnit, ProviderError, SchedulerId,
};
use js_provider_lambdalabs::{
    Architecture, ElasticConfig, FakeLambda, Instance, InstanceId, InstanceTypeOffer,
    LambdaProvider, ManualClock, OptionTable, Region, Tag,
};
use js_types::{Accelerator, ExecMode, Interconnect, Model, accelerator_of};

/// The provider's catalog, awaited on a runtime of its own: the catalog runs its blocking
/// call on tokio's blocking pool, which needs a runtime.
pub fn catalog(provider: &LambdaProvider) -> Result<Vec<Offer>, ProviderError> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .build()
        .unwrap();
    runtime.block_on(provider.catalog())
}

pub const SCHEDULER: &str = "sched-one";
pub const KEY: &str = "fixture-key";

/// The interval the rigs use between launches.
pub const INTERVAL: Duration = Duration::from_secs(12);

/// The allow list most tests use: two instance types of one GPU model, the H100 SXM5.
pub const TABLE: &str = r#"
[[option]]
instance_type = "gpu_1x_h100_sxm5"

[[option]]
instance_type = "gpu_8x_h100_sxm5"
"#;

/// The canonical model of the rig's GPUs, as a request names it.
pub const H100: &str = "nvidia-h100";

/// The accelerator the catalog table gives `count` H100 SXM5 GPUs: the `nvidia-smi`
/// spelling of the variant, and the NVLink domain of the eight-GPU board.
pub fn h100(count: u32) -> Accelerator {
    let mut accelerator =
        accelerator_of(&Model::parse("nvidia-h100-80gb-hbm3").unwrap(), count, None);
    if count == 8 {
        accelerator.interconnect = Some(Interconnect::parse("nvlink-domain>=8").unwrap());
    }
    accelerator
}

pub fn offer(
    name: &str,
    price: u64,
    gpus: u32,
    arch: Architecture,
    regions: &[&str],
) -> InstanceTypeOffer {
    InstanceTypeOffer {
        name: name.to_owned(),
        description: String::new(),
        gpu_description: String::new(),
        price_cents_per_hour: price,
        vcpus: 8 * gpus,
        memory_gib: 64 * gpus,
        storage_gib: 100,
        gpus,
        architecture: arch,
        regions: regions
            .iter()
            .map(|name| Region {
                name: (*name).to_owned(),
                description: String::new(),
            })
            .collect(),
    }
}

/// The instance types the rig's account offers: the two types of [`TABLE`], each with
/// the GPU count the catalog table gives it.
pub fn offers() -> Vec<InstanceTypeOffer> {
    vec![
        offer(
            "gpu_1x_h100_sxm5",
            75,
            1,
            Architecture::X86_64,
            &["region-b", "region-a"],
        ),
        offer(
            "gpu_8x_h100_sxm5",
            1200,
            8,
            Architecture::X86_64,
            &["region-a"],
        ),
    ]
}

pub struct Rig {
    pub fake: Arc<FakeLambda>,
    pub clock: Arc<ManualClock>,
    pub provider: LambdaProvider,
}

/// The config of a live, enabled provider with room for `max_machines` machines.
pub fn live(max_machines: usize) -> ElasticConfig {
    config(max_machines)
        .enable()
        .live()
        .launch_interval(INTERVAL)
}

/// The config a provider starts from: disabled and in dry-run mode.
pub fn config(max_machines: usize) -> ElasticConfig {
    ElasticConfig::new(
        SchedulerId::new(SCHEDULER).unwrap(),
        KEY,
        NonZeroUsize::new(max_machines).unwrap(),
    )
    .unwrap()
}

pub fn rig(config: ElasticConfig) -> Rig {
    rig_with(config, TABLE)
}

/// Installs a subscriber that takes every event and writes none: a disabled event does
/// not evaluate its fields, and the fields are code that must run once under test.
fn trace_everything() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(std::io::sink)
            .try_init();
    });
}

pub fn rig_with(config: ElasticConfig, table: &str) -> Rig {
    trace_everything();
    let fake = Arc::new(FakeLambda::new());
    fake.set_offers(offers());
    let clock = Arc::new(ManualClock::new());
    let provider = LambdaProvider::new(
        fake.clone(),
        clock.clone(),
        OptionTable::parse(table).unwrap(),
        config,
    );
    Rig {
        fake,
        clock,
        provider,
    }
}

/// The offer of `gpu_1x_h100_sxm5` in `region`, as the catalog gives it.
pub fn candidate(region: &str) -> Offer {
    Offer {
        id: "gpu_1x_h100_sxm5".to_owned(),
        zone: Some(region.to_owned()),
        cpu_millis: 8000,
        memory_bytes: 64 << 30,
        disk_bytes: 100 << 30,
        accelerator: Some(h100(1)),
        os: "linux".to_owned(),
        arch: "x86_64".to_owned(),
        isa_level: None,
        exec: ExecMode::Container,
        price_cents_per_hour: Some(75),
        price_unit: PriceUnit::UsCents,
        available: true,
        purchase: Some(js_elastic::Purchase::OnDemand),
        quota: None,
        block: None,
    }
}

/// An acquire of the rig's scheduler, with the name it derives and no payload.
pub fn ask(acquire: &str) -> AcquireRequest {
    ask_as(SCHEDULER, acquire)
}

/// An acquire as the scheduler `scheduler` makes it.
pub fn ask_as(scheduler: &str, acquire: &str) -> AcquireRequest {
    let acquire = AcquireId::new(acquire).unwrap();
    AcquireRequest {
        name: HostName::derive(&SchedulerId::new(scheduler).unwrap(), &acquire),
        acquire,
        pool: "pool-a".to_owned(),
        lifetime: None,
        offer: candidate("region-a"),
        labels: Vec::new(),
        payload: BootPayload::new(String::new()),
    }
}

pub fn id(text: &str) -> InstanceId {
    InstanceId::new(text).unwrap()
}

/// The scheduler's reference to the instance `text`.
pub fn host(text: &str) -> HostRef {
    id(text).host_ref()
}

pub fn tag(key: &str, value: &str) -> Tag {
    Tag {
        key: key.to_owned(),
        value: value.to_owned(),
    }
}

/// An instance of the account with these tags and name, as `GET /instances` shows it.
pub fn row(id_text: &str, name: Option<&str>, status: HostStatus, tags: Vec<Tag>) -> Instance {
    Instance {
        id: id(id_text),
        name: name.map(str::to_owned),
        status,
        region: "region-a".to_owned(),
        instance_type: "gpu_1x_h100_sxm5".to_owned(),
        ip: None,
        private_ip: None,
        hostname: None,
        first_healthy: None,
        tags,
    }
}

/// A writer into a buffer the test reads back.
#[derive(Clone, Default)]
struct Lines(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Lines {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Lines {
    type Writer = Self;

    fn make_writer(&'a self) -> Self {
        self.clone()
    }
}

/// What `run` returns, and the warnings it logged on this thread, one line each.
pub fn warnings<T>(run: impl FnOnce() -> T) -> (T, String) {
    let lines = Lines::default();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .with_writer(lines.clone())
        .finish();
    let out = tracing::subscriber::with_default(subscriber, run);
    let text = String::from_utf8(lines.0.lock().unwrap().clone()).unwrap();
    (out, text)
}
