//! A provider over the in-memory project, with a clock that moves only when told to. The
//! project, zones, images and networks are made up, and the made-up machine types
//! (`q9-*`) are not Compute Engine's.

use std::collections::BTreeSet;
use std::num::NonZeroUsize;
use std::ops::Deref;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use js_elastic::{
    AcquireId, AcquireRequest, BootPayload, HostName, HostRef, Offer, PriceUnit, Purchase,
    SchedulerId,
};
use js_provider_gce::{
    AcceleratorTypeInfo, ApiError, ArchName, AttachedAccelerator, FakeGce, GceApi,
    GceCatalogSettings, GceConfig, GceProvider, ImagePath, Images, InsertRequest, Instance,
    InstanceId, InstanceRef, InstanceStatus, Label, ListFilter, MachineTypeInfo, MachineTypeName,
    ManualClock, Money, NetworkInterface, Operation, OperationRef, Page, PricedItem, Project,
    QuotaMetric, Region, SeriesName, SkuPrice, UsageType, Zone, host_ref, label_value,
};
use js_types::ExecMode;

pub const SCHEDULER: &str = "sched-one";
pub const PROJECT: &str = "sample-project";
pub const ZONE: &str = "region-one-a";
pub const OTHER_ZONE: &str = "region-one-b";
pub const X86_IMAGE: &str = "projects/image-project/global/images/js-agent-x86";
pub const ARM_IMAGE: &str = "projects/image-project/global/images/family/js-agent-arm";
pub const NETWORK: &str = "global/networks/default";
pub const SUBNETWORK: &str = "regions/region-one/subnetworks/default";

/// The rig's x86_64 CPU type, offered on demand and on Spot in [`ZONE`].
pub const CPU_TYPE: &str = "q9-standard-2";
/// The rig's arm64 CPU type, offered on demand and on Spot in [`ZONE`].
pub const ARM_TYPE: &str = "q9a-standard-2";
/// The rig's GPU type: two A100s of 40 GiB built in, as the translation table knows them.
pub const GPU_TYPE: &str = "a2-highgpu-2g";

/// How long the provider holds an insert that got no answer, unless a test sets it.
pub const HOLD: Duration = Duration::from_secs(600);

/// The in-memory project, and the faults a test can add that it does not script itself: a
/// list that ignores its label filter, and a catalog call that panics.
pub struct Account {
    fake: FakeGce,
    unfiltered: AtomicBool,
    panic_once: AtomicBool,
}

impl Account {
    /// A project whose catalog holds [`CPU_TYPE`], [`ARM_TYPE`] and [`GPU_TYPE`], each in
    /// [`ZONE`] and priced on demand and on Spot (the GPU type only: the `q9` series has no
    /// billing row, so its offers are listed unpriced).
    pub fn new() -> Self {
        let fake = FakeGce::new();
        let zone = zone(ZONE);
        fake.add_machine_type(machine(CPU_TYPE, 2, &[], "X86_64"))
            .add_machine_type(machine(ARM_TYPE, 2, &[], "ARM64"))
            .add_machine_type(machine(GPU_TYPE, 24, &[("nvidia-tesla-a100", 2)], "X86_64"));
        let region = zone.region();
        for usage in [UsageType::OnDemand, UsageType::Preemptible] {
            fake.add_price(
                &region,
                sku(PricedItem::Cores(series("a2")), usage, 500_000_000),
            )
            .add_price(
                &region,
                sku(PricedItem::Memory(series("a2")), usage, 50_000_000),
            )
            .add_price(
                &region,
                sku(
                    PricedItem::Accelerator(js_provider_gce::AcceleratorTypeName(
                        "nvidia-tesla-a100".to_owned(),
                    )),
                    usage,
                    900_000_000,
                ),
            );
        }
        Self {
            fake,
            unfiltered: AtomicBool::new(false),
            panic_once: AtomicBool::new(false),
        }
    }

    /// Every list from now on answers every instance of the project, whatever its labels.
    pub fn ignore_label_filters(&self) {
        self.unfiltered.store(true, Ordering::SeqCst);
    }

    /// The next read of the machine types panics.
    pub fn panic_on_next_catalog(&self) {
        self.panic_once.store(true, Ordering::SeqCst);
    }
}

impl Default for Account {
    fn default() -> Self {
        Self::new()
    }
}

impl Deref for Account {
    type Target = FakeGce;

    fn deref(&self) -> &FakeGce {
        &self.fake
    }
}

impl GceApi for Account {
    fn machine_types(
        &self,
        project: &Project,
        zones: &[Zone],
    ) -> Result<Vec<MachineTypeInfo>, ApiError> {
        assert!(
            !self.panic_once.swap(false, Ordering::SeqCst),
            "the call died"
        );
        self.fake.machine_types(project, zones)
    }

    fn accelerator_types(
        &self,
        project: &Project,
        zones: &[Zone],
    ) -> Result<Vec<AcceleratorTypeInfo>, ApiError> {
        self.fake.accelerator_types(project, zones)
    }

    fn prices(&self, region: &Region) -> Result<Vec<SkuPrice>, ApiError> {
        self.fake.prices(region)
    }

    fn region_quotas(
        &self,
        project: &Project,
        region: &Region,
    ) -> Result<Vec<QuotaMetric>, ApiError> {
        self.fake.region_quotas(project, region)
    }

    fn insert_instance(&self, request: &InsertRequest) -> Result<Operation, ApiError> {
        self.fake.insert_instance(request)
    }

    fn get_instance(&self, instance: &InstanceRef) -> Result<Instance, ApiError> {
        self.fake.get_instance(instance)
    }

    fn delete_instance(&self, instance: &InstanceRef) -> Result<Operation, ApiError> {
        self.fake.delete_instance(instance)
    }

    fn list_instances(&self, filter: &ListFilter) -> Result<Page<Instance>, ApiError> {
        match self.unfiltered.load(Ordering::SeqCst) {
            true => self.fake.list_instances(&ListFilter {
                labels: Vec::new(),
                ..filter.clone()
            }),
            false => self.fake.list_instances(filter),
        }
    }

    fn get_operation(&self, operation: &OperationRef) -> Result<Operation, ApiError> {
        self.fake.get_operation(operation)
    }
}

fn series(name: &str) -> SeriesName {
    SeriesName(name.to_owned())
}

fn sku(item: PricedItem, usage: UsageType, nanos: i32) -> SkuPrice {
    SkuPrice {
        item,
        usage,
        unit_price: Money {
            currency: "USD".to_owned(),
            units: "0".to_owned(),
            nanos,
        },
    }
}

/// A machine type in [`ZONE`] with `vcpus` vCPUs and 4 GiB per vCPU, the built-in
/// `accelerators` and the stated `arch`.
pub fn machine(
    name: &str,
    vcpus: u32,
    accelerators: &[(&str, u32)],
    arch: &str,
) -> MachineTypeInfo {
    MachineTypeInfo {
        name: MachineTypeName(name.to_owned()),
        zone: zone(ZONE),
        vcpus,
        memory_mib: u64::from(vcpus) * 4096,
        accelerators: accelerators
            .iter()
            .map(|(type_name, count)| AttachedAccelerator {
                type_name: js_provider_gce::AcceleratorTypeName((*type_name).to_owned()),
                count: *count,
            })
            .collect(),
        arch: Some(ArchName(arch.to_owned())),
    }
}

pub struct Rig {
    pub account: Arc<Account>,
    pub clock: Arc<ManualClock>,
    pub provider: GceProvider,
}

pub fn zone(name: &str) -> Zone {
    Zone::new(name).unwrap()
}

pub fn image(text: &str) -> ImagePath {
    ImagePath::new(text).unwrap()
}

/// The images of the rig: one for each architecture.
pub fn images() -> Images {
    Images::new(Some(image(ARM_IMAGE)), Some(image(X86_IMAGE))).unwrap()
}

/// The network every instance of the rig is attached to.
pub fn network() -> NetworkInterface {
    NetworkInterface {
        network: NETWORK.to_owned(),
        subnetwork: SUBNETWORK.to_owned(),
    }
}

/// The config a provider starts from: disabled and in dry-run mode.
pub fn config(max_machines: usize) -> GceConfig {
    config_with(max_machines, images())
}

/// The config of a disabled, dry-run provider with these images.
pub fn config_with(max_machines: usize, images: Images) -> GceConfig {
    GceConfig::new(
        SchedulerId::new(SCHEDULER).unwrap(),
        Project::new(PROJECT).unwrap(),
        images,
        network(),
        NonZeroUsize::new(max_machines).unwrap(),
    )
}

/// The config of a live, enabled provider with room for `max_machines` machines.
pub fn live(max_machines: usize) -> GceConfig {
    config(max_machines).enable().live()
}

/// The config of a live, enabled provider whose only image is the x86_64 one.
pub fn live_x86_only(max_machines: usize) -> GceConfig {
    config_with(
        max_machines,
        Images::new(None, Some(image(X86_IMAGE))).unwrap(),
    )
    .enable()
    .live()
}

/// The catalog settings of the rig: on demand and Spot, the rig's three types, in
/// [`ZONE`] and [`OTHER_ZONE`].
pub fn settings() -> GceCatalogSettings {
    let names = [CPU_TYPE, ARM_TYPE, GPU_TYPE];
    GceCatalogSettings::new(
        PROJECT,
        &[ZONE, OTHER_ZONE],
        &["on-demand", "spot"],
        Some(names.iter().map(|name| (*name).to_owned()).collect()),
    )
    .unwrap()
}

pub fn rig(config: GceConfig) -> Rig {
    rig_over(Arc::new(Account::new()), config)
}

/// Installs a subscriber that takes every event and writes none: a disabled event does
/// not evaluate its fields, and the fields are code that must run once under test.
pub fn trace_everything() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(std::io::sink)
            .try_init();
    });
}

pub fn rig_over(account: Arc<Account>, config: GceConfig) -> Rig {
    trace_everything();
    let clock = Arc::new(ManualClock::new());
    let provider = GceProvider::new(account.clone(), clock.clone(), settings(), config);
    Rig {
        account,
        clock,
        provider,
    }
}

/// An offer with the id `id` and the architecture `arch`, bought as `purchase`, in
/// [`ZONE`].
pub fn offer_as(id: &str, arch: &str, purchase: Purchase) -> Offer {
    Offer {
        id: id.to_owned(),
        zone: Some(ZONE.to_owned()),
        cpu_millis: 2000,
        memory_bytes: 8 << 30,
        disk_bytes: 0,
        accelerator: None,
        os: "linux".to_owned(),
        arch: arch.to_owned(),
        isa_level: None,
        exec: ExecMode::Container,
        price_cents_per_hour: Some(50),
        price_unit: PriceUnit::UsCents,
        available: true,
        purchase: Some(purchase),
        quota: None,
        block: None,
    }
}

/// An offer with the id `id` and the architecture `arch`, on demand, in [`ZONE`].
pub fn offer(id: &str, arch: &str) -> Offer {
    offer_as(id, arch, Purchase::OnDemand)
}

/// The offer the rig's acquires are for: [`CPU_TYPE`] on demand in [`ZONE`], x86_64.
pub fn candidate() -> Offer {
    offer(&format!("{CPU_TYPE}/on-demand"), "x86_64")
}

/// An acquire of the rig's scheduler, with the name it derives and the payload
/// `user-data`.
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
        offer: candidate(),
        labels: Vec::new(),
        payload: BootPayload::new("user-data".to_owned()),
    }
}

pub fn id(text: &str) -> InstanceId {
    InstanceId::new(text).unwrap()
}

/// The scheduler's reference to the instance `text` of [`ZONE`].
pub fn host(text: &str) -> HostRef {
    host_ref(&zone(ZONE), &id(text))
}

pub fn label(key: &str, value: &str) -> Label {
    Label {
        key: key.to_owned(),
        value: value.to_owned(),
    }
}

/// The name `scheduler` derives for `acquire`.
pub fn name_of(scheduler: &str, acquire: &str) -> String {
    let scheduler = SchedulerId::new(scheduler).unwrap();
    HostName::derive(&scheduler, &AcquireId::new(acquire).unwrap()).to_string()
}

/// The two labels an insert for `acquire` by `scheduler` carries.
pub fn marks(scheduler: &str, acquire: &str) -> Vec<Label> {
    vec![
        label("js-scheduler", &label_value(scheduler)),
        label("js-acquire-id", &label_value(acquire)),
    ]
}

/// An instance of the project in [`ZONE`] in `status`, named `name`, with these labels,
/// of the rig's CPU type.
pub fn row(id_text: &str, status: InstanceStatus, name: &str, labels: Vec<Label>) -> Instance {
    Instance {
        id: id(id_text),
        name: name.to_owned(),
        status,
        labels,
        machine_type: MachineTypeName(CPU_TYPE.to_owned()),
        zone: zone(ZONE),
    }
}

/// An instance this scheduler launched for `acquire`, as the project shows it.
pub fn ours(id_text: &str, status: InstanceStatus, acquire: &str) -> Instance {
    row(
        id_text,
        status,
        &name_of(SCHEDULER, acquire),
        marks(SCHEDULER, acquire),
    )
}

/// The id of the `n`th instance nothing of the rig launched: `9000` and `n`, far above the
/// ids the fake gives its own inserts.
pub fn stranger(n: u8) -> String {
    format!("9000{n:03}")
}

/// A set of text, for comparing without order.
pub fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

/// A writer into a buffer the test reads back.
#[derive(Clone, Default)]
struct Lines(Arc<Mutex<Vec<u8>>>);

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

/// What `run` returns, and the events up to `level` it logged on this thread, one line
/// each.
pub fn logged<T>(level: tracing::Level, run: impl FnOnce() -> T) -> (T, String) {
    let lines = Lines::default();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(level)
        .with_writer(lines.clone())
        .finish();
    let out = tracing::subscriber::with_default(subscriber, run);
    let text = String::from_utf8(lines.0.lock().unwrap().clone()).unwrap();
    (out, text)
}
