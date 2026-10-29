//! A provider over the in-memory account, with a clock that moves only when told to.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use js_elastic::{
    AcquireId, AcquireRequest, BootPayload, GpuClass, HostName, HostRef, HostStatus, Offer,
    PriceUnit, SchedulerId,
};
use js_provider_lambdalabs::{
    Architecture, ElasticConfig, FakeLambda, Instance, InstanceId, InstanceTypeOffer,
    LambdaProvider, ManualClock, OptionTable, Region, Tag,
};
use js_types::ExecMode;

pub const SCHEDULER: &str = "sched-one";
pub const KEY: &str = "fixture-key";

/// The interval the rigs use between launches.
pub const INTERVAL: Duration = Duration::from_secs(12);

pub const TABLE: &str = r#"
[[option]]
instance_type = "gpu_1x_sample"
gpu_class = "nvidia-sample"
gpus = 1

[[option]]
instance_type = "gpu_8x_example"
gpu_class = "nvidia-sample"
gpus = 8
"#;

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
    fake.set_offers(vec![
        offer(
            "gpu_1x_sample",
            75,
            1,
            Architecture::X86_64,
            &["region-b", "region-a"],
        ),
        offer(
            "gpu_8x_example",
            1200,
            8,
            Architecture::X86_64,
            &["region-a"],
        ),
    ]);
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

/// The offer of `gpu_1x_sample` in `region`, as the catalog gives it.
pub fn candidate(region: &str) -> Offer {
    Offer {
        id: "gpu_1x_sample".to_owned(),
        zone: Some(region.to_owned()),
        cpu_millis: 8000,
        memory_bytes: 64 << 30,
        disk_bytes: 100 << 30,
        gpus: 1,
        gpu_class: Some(GpuClass::parse("nvidia-sample").unwrap()),
        os: "linux".to_owned(),
        arch: "x86_64".to_owned(),
        exec: ExecMode::Container,
        price_cents_per_hour: Some(75),
        price_unit: PriceUnit::UsCents,
        available: true,
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
        instance_type: "gpu_1x_sample".to_owned(),
        ip: None,
        private_ip: None,
        hostname: None,
        first_healthy: None,
        tags,
    }
}
