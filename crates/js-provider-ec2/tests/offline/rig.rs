//! A provider over the in-memory account, with a clock that moves only when told to.

use std::collections::BTreeSet;
use std::num::NonZeroUsize;
use std::ops::Deref;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use js_elastic::{
    AcquireId, AcquireRequest, BootPayload, HostName, HostRef, Offer, PriceUnit, ProviderError,
    SchedulerId,
};
use js_provider_ec2::{
    ApiError, ArchName, BlockOffering, BlockPurchase, CapacityReservation, DescribeFilter, Ec2Api,
    Ec2CatalogSettings, Ec2Config, Ec2Provider, FakeEc2, GpuInfo, ImageId, Images, Instance,
    InstanceId, InstanceState, InstanceType, InstanceTypeInfo, ManualClock, PurchaseKind,
    QuotaFamily, Region, RunRequest, Tag, TypePrice, VcpuQuota, Zone, ZoneOffering, ZonePrice,
};
use js_types::ExecMode;

pub const SCHEDULER: &str = "sched-one";
pub const REGION: &str = "region-a";
pub const ZONE: &str = "zone-a1";
pub const X86_IMAGE: &str = "ami-0123456789abcdef0";
pub const ARM_IMAGE: &str = "ami-0123456789abcdef1";

/// The rig's x86_64 CPU type, offered on demand and on spot in [`ZONE`].
pub const CPU_TYPE: &str = "m9.large";
/// The rig's arm64 CPU type, offered on demand in [`ZONE`].
pub const ARM_TYPE: &str = "c9g.large";
/// The rig's GPU type: eight A100s of 40 GiB, as the translation table knows them.
pub const GPU_TYPE: &str = "p9.24xlarge";

/// How long the provider holds a launch that got no answer, unless a test sets it.
pub const HOLD: Duration = Duration::from_secs(600);

/// The in-memory account, and the faults a test can add that it does not script itself: an
/// answer to a launch that names more instances, or none, a tag read that ignores its
/// filter, a terminate after which the instance is gone, and a catalog call that panics.
pub struct Account {
    fake: FakeEc2,
    extra: Mutex<Vec<InstanceId>>,
    empty_run: AtomicBool,
    unfiltered: AtomicBool,
    vanish: AtomicBool,
    panic_once: AtomicBool,
}

impl Account {
    /// An account whose catalog holds [`CPU_TYPE`], [`ARM_TYPE`] and [`GPU_TYPE`], each
    /// offered in [`ZONE`] and priced on demand, and [`CPU_TYPE`] on spot too.
    pub fn new() -> Self {
        let fake = FakeEc2::new();
        fake.add_instance_type(kind(CPU_TYPE, 2, "x86_64", Vec::new()))
            .add_instance_type(kind(ARM_TYPE, 2, "arm64", Vec::new()))
            .add_instance_type(kind(
                GPU_TYPE,
                96,
                "x86_64",
                vec![GpuInfo {
                    name: "A100".to_owned(),
                    manufacturer: "NVIDIA".to_owned(),
                    count: 8,
                    memory_mib: 40 * 1024,
                }],
            ));
        for name in [CPU_TYPE, ARM_TYPE, GPU_TYPE] {
            fake.add_zone_offering(name, ZONE)
                .add_on_demand_price(name, "0.5");
        }
        fake.add_spot_price(CPU_TYPE, ZONE, "0.2");
        Self {
            fake,
            extra: Mutex::new(Vec::new()),
            empty_run: AtomicBool::new(false),
            unfiltered: AtomicBool::new(false),
            vanish: AtomicBool::new(false),
            panic_once: AtomicBool::new(false),
        }
    }

    /// The next launch that takes effect also names `ids`, which the account holds or not.
    pub fn name_also(&self, ids: Vec<InstanceId>) {
        *self.extra.lock().unwrap() = ids;
    }

    /// Every launch from now on takes effect and answers that it started nothing.
    pub fn answer_no_instance(&self) {
        self.empty_run.store(true, Ordering::SeqCst);
    }

    /// Every tag read from now on answers every instance of the account, whatever the tags.
    pub fn ignore_tag_filters(&self) {
        self.unfiltered.store(true, Ordering::SeqCst);
    }

    /// Every terminate from now on takes effect and leaves the instance unknown to the
    /// cloud at once.
    pub fn forget_after_terminate(&self) {
        self.vanish.store(true, Ordering::SeqCst);
    }

    /// The next read of the instance types panics.
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
    type Target = FakeEc2;

    fn deref(&self) -> &FakeEc2 {
        &self.fake
    }
}

impl Ec2Api for Account {
    fn instance_types(&self, region: &Region) -> Result<Vec<InstanceTypeInfo>, ApiError> {
        assert!(
            !self.panic_once.swap(false, Ordering::SeqCst),
            "the call died"
        );
        self.fake.instance_types(region)
    }

    fn zone_offerings(&self, region: &Region) -> Result<Vec<ZoneOffering>, ApiError> {
        self.fake.zone_offerings(region)
    }

    fn on_demand_prices(&self, region: &Region) -> Result<Vec<TypePrice>, ApiError> {
        self.fake.on_demand_prices(region)
    }

    fn spot_prices(&self, region: &Region) -> Result<Vec<ZonePrice>, ApiError> {
        self.fake.spot_prices(region)
    }

    fn vcpu_quota(
        &self,
        region: &Region,
        family: QuotaFamily,
        purchase: PurchaseKind,
    ) -> Result<Option<VcpuQuota>, ApiError> {
        self.fake.vcpu_quota(region, family, purchase)
    }

    fn capacity_block_offerings(
        &self,
        region: &Region,
        hours: u32,
    ) -> Result<Vec<BlockOffering>, ApiError> {
        self.fake.capacity_block_offerings(region, hours)
    }

    fn run_instances(&self, request: &RunRequest) -> Result<Vec<InstanceId>, ApiError> {
        let mut ids = self.fake.run_instances(request)?;
        ids.append(&mut self.extra.lock().unwrap());
        match self.empty_run.load(Ordering::SeqCst) {
            true => Ok(Vec::new()),
            false => Ok(ids),
        }
    }

    fn describe_instances(&self, filter: &DescribeFilter) -> Result<Vec<Instance>, ApiError> {
        let everything = DescribeFilter::Tags(Vec::new());
        match (filter, self.unfiltered.load(Ordering::SeqCst)) {
            (DescribeFilter::Tags(_), true) => self.fake.describe_instances(&everything),
            _ => self.fake.describe_instances(filter),
        }
    }

    fn purchase_capacity_block(
        &self,
        request: &BlockPurchase,
    ) -> Result<CapacityReservation, ApiError> {
        self.fake.purchase_capacity_block(request)
    }

    fn capacity_reservations(&self, tags: &[Tag]) -> Result<Vec<CapacityReservation>, ApiError> {
        self.fake.capacity_reservations(tags)
    }

    fn terminate_instances(&self, ids: &[InstanceId]) -> Result<(), ApiError> {
        self.fake.terminate_instances(ids)?;
        if self.vanish.load(Ordering::SeqCst) {
            ids.iter().for_each(|id| {
                self.fake.remove_instance(id);
            });
        }
        Ok(())
    }
}

/// An instance type of `vcpus` vCPUs and 4 GiB per vCPU on the architecture `arch`.
pub fn kind(name: &str, vcpus: u32, arch: &str, gpus: Vec<GpuInfo>) -> InstanceTypeInfo {
    InstanceTypeInfo {
        name: InstanceType(name.to_owned()),
        vcpus,
        memory_mib: u64::from(vcpus) * 4096,
        architectures: vec![ArchName(arch.to_owned())],
        gpus,
        instance_storage_gb: None,
        bare_metal: false,
    }
}

pub struct Rig {
    pub account: Arc<Account>,
    pub clock: Arc<ManualClock>,
    pub provider: Ec2Provider,
}

/// The images of the rig: one for each architecture.
pub fn images() -> Images {
    Images::new(Some(image(ARM_IMAGE)), Some(image(X86_IMAGE))).unwrap()
}

pub fn image(text: &str) -> ImageId {
    ImageId::new(text).unwrap()
}

/// The config a provider starts from: disabled and in dry-run mode.
pub fn config(max_machines: usize) -> Ec2Config {
    Ec2Config::new(
        SchedulerId::new(SCHEDULER).unwrap(),
        Region::new(REGION).unwrap(),
        images(),
        NonZeroUsize::new(max_machines).unwrap(),
    )
}

/// The config of a live, enabled provider with room for `max_machines` machines.
pub fn live(max_machines: usize) -> Ec2Config {
    config(max_machines).enable().live()
}

/// The config of a live, enabled provider whose only image is the x86_64 one.
pub fn live_x86_only(max_machines: usize) -> Ec2Config {
    Ec2Config::new(
        SchedulerId::new(SCHEDULER).unwrap(),
        Region::new(REGION).unwrap(),
        Images::new(None, Some(image(X86_IMAGE))).unwrap(),
        NonZeroUsize::new(max_machines).unwrap(),
    )
    .enable()
    .live()
}

/// The catalog settings of the rig: on demand and spot, the rig's three types.
pub fn settings() -> Ec2CatalogSettings {
    let names = [CPU_TYPE, ARM_TYPE, GPU_TYPE, "g9g.xlarge"];
    Ec2CatalogSettings::new(
        REGION,
        &["on-demand", "spot"],
        Some(names.iter().map(|name| (*name).to_owned()).collect()),
        None,
    )
    .unwrap()
}

pub fn rig(config: Ec2Config) -> Rig {
    rig_over(Arc::new(Account::new()), config)
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

pub fn rig_over(account: Arc<Account>, config: Ec2Config) -> Rig {
    trace_everything();
    let clock = Arc::new(ManualClock::new());
    let provider = Ec2Provider::new(account.clone(), clock.clone(), settings(), config);
    Rig {
        account,
        clock,
        provider,
    }
}

/// The provider's catalog, awaited on a runtime of its own: the catalog runs its blocking
/// calls on tokio's blocking pool, which needs a runtime.
pub fn catalog(provider: &Ec2Provider) -> Result<Vec<Offer>, ProviderError> {
    use js_elastic::CapacityProvider;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .build()
        .unwrap();
    runtime.block_on(provider.catalog())
}

/// An offer of the rig's on-demand `m9.large` in [`ZONE`], as the catalog gives it.
pub fn candidate() -> Offer {
    offer(&format!("{CPU_TYPE}/on-demand"), "x86_64")
}

/// An offer with the id `id` and the architecture `arch`, on demand, in [`ZONE`].
pub fn offer(id: &str, arch: &str) -> Offer {
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
        purchase: Some(js_elastic::Purchase::OnDemand),
        quota: None,
        block: None,
    }
}

/// The id of the rig's Capacity Block offering.
pub const BLOCK_OFFERING: &str = "cb-0123456789abcdef0";

/// When the rig's block starts, in seconds since the epoch.
pub const BLOCK_START: u64 = 1_800_000_000;

/// How long the rig's block lasts, in minutes (one day).
pub const BLOCK_MINUTES: u64 = 1440;

/// When the rig's block ends, in seconds since the epoch.
pub const BLOCK_END: u64 = BLOCK_START + BLOCK_MINUTES * 60;

/// The Capacity Block offering of [`CPU_TYPE`] in [`ZONE`] the account can be given.
pub fn block_offering() -> BlockOffering {
    BlockOffering {
        id: BLOCK_OFFERING.to_owned(),
        instance_type: InstanceType(CPU_TYPE.to_owned()),
        zone: Zone(ZONE.to_owned()),
        instance_count: 1,
        start_unix_seconds: BLOCK_START,
        duration_minutes: BLOCK_MINUTES,
        upfront_fee: "100.00".to_owned(),
        currency: "USD".to_owned(),
    }
}

/// The offer the catalog makes of [`block_offering`].
pub fn block_offer() -> Offer {
    Offer {
        purchase: Some(js_elastic::Purchase::CapacityBlock),
        block: Some(js_elastic::BlockTerms {
            offering: BLOCK_OFFERING.to_owned(),
            start_unix_seconds: BLOCK_START,
            duration_minutes: BLOCK_MINUTES,
            upfront_cents: 10_000,
            instances: 1,
        }),
        ..offer(
            &format!("{CPU_TYPE}/capacity-block/{BLOCK_OFFERING}"),
            "x86_64",
        )
    }
}

/// An acquire of the rig's scheduler for [`block_offer`].
pub fn ask_block(acquire: &str) -> AcquireRequest {
    AcquireRequest {
        offer: block_offer(),
        ..ask(acquire)
    }
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

/// The name the rig's scheduler derives for `acquire`.
pub fn name_of(scheduler: &str, acquire: &str) -> String {
    let scheduler = SchedulerId::new(scheduler).unwrap();
    HostName::derive(&scheduler, &AcquireId::new(acquire).unwrap()).to_string()
}

/// The three tags a launch for `acquire` by `scheduler` carries.
pub fn marks(scheduler: &str, acquire: &str) -> Vec<Tag> {
    vec![
        tag("js-scheduler", scheduler),
        tag("js-acquire-id", acquire),
        tag("Name", &name_of(scheduler, acquire)),
    ]
}

/// An instance of the account in `state` with these tags, of the rig's CPU type and x86_64
/// image.
pub fn row(id_text: &str, state: InstanceState, tags: Vec<Tag>) -> Instance {
    Instance {
        id: id(id_text),
        state,
        tags,
        instance_type: InstanceType(CPU_TYPE.to_owned()),
        zone: Zone(ZONE.to_owned()),
        image: image(X86_IMAGE),
    }
}

/// An instance this scheduler launched for `acquire`, as the account shows it.
pub fn ours(id_text: &str, state: InstanceState, acquire: &str) -> Instance {
    row(id_text, state, marks(SCHEDULER, acquire))
}

/// The ids of instances nothing launched: `i-` and 17 hexadecimal digits that end in `n`.
pub fn stranger(n: u8) -> String {
    format!("i-0000000000000f{n:03x}")
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
