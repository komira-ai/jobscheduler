//! An in-process server whose live pool `burst` reaches a static provider and two launching
//! ones, `first` and then `second`, each over its own elastic fake cloud (design 5.6, step
//! 5): the queued job's machine is asked of `first`; a `capacity` refusal falls through to
//! `second`; once both have refused, the job waits `acquire_retry` and `first`, recovered,
//! launches it. The clouds' acquires are scripted; time is the test's clock, and nothing
//! sleeps.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use js_elastic::{
    AcquireId, AcquireRequest, Acquired, BootStep, CapacityProvider, CatalogFuture, CheckResult,
    Held, HostRef, Offer, ProviderError, RefusalCode, Released,
};
use js_elastic_fake::{Billing, Call, ElasticFake, FakeClock, Step};
use js_lease_server::in_process::InProcess;
use js_lease_server::{
    BuildCtx, Built, Config, ElasticBuilt, Floor, KindError, Kinds, OwnerOnlyFiles,
};
use js_types::{ExecMode, ManualClock, Model, ProviderId};

use crate::elastic::{file_text, offer, queue_job};
use crate::support::{Scratch, Stepping, clock};

/// An elastic fake cloud under the provider id `id`, so two of them in one pool are told
/// apart.
struct Named {
    id: ProviderId,
    cloud: ElasticFake,
}

impl CapacityProvider for Named {
    fn id(&self) -> ProviderId {
        self.id.clone()
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
        self.cloud.checks()
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
}

/// The clouds of these tests, by the path of the key file that names them: a kind is a
/// plain function, so a test leaves its clouds here for the kind to find.
static CLOUDS: Mutex<BTreeMap<String, (ProviderId, ElasticFake)>> = Mutex::new(BTreeMap::new());

/// The `named-cloud` kind: the cloud a test left in [`CLOUDS`] for the machine provider's
/// key file, behind [`Named`]. Its floor is one `nvidia-a100`, on hosts that run containers.
fn named_cloud(table: &toml::Table, _ctx: &BuildCtx<'_>) -> Result<Built, KindError> {
    let path = table.get("api_key_file").and_then(toml::Value::as_str);
    let clouds = CLOUDS.lock().unwrap();
    let (id, cloud) = clouds.get(path.unwrap()).cloned().unwrap();
    Ok(Built::Elastic(ElasticBuilt {
        idle_costs: false,
        provider: Box::new(Named { id, cloud }),
        floor: Floor {
            gpus: 1,
            gpu_class: Some(Model::parse("nvidia-a100").unwrap()),
        },
        exec: vec![ExecMode::Container],
    }))
}

/// A started server, its clock and its two clouds; the scratch directory holds its files.
struct Started {
    server: InProcess,
    clock: ManualClock,
    first: ElasticFake,
    second: ElasticFake,
    _scratch: Scratch,
}

/// A fresh cloud that lists the one offer of [`offer`], left in [`CLOUDS`] under the key
/// file `key` of `scratch` with the id `id`; returns the key file's path.
fn cloud(scratch: &Scratch, key: &str, id: &'static str) -> (String, ElasticFake) {
    let path = scratch
        .write(key, "test-api-key", 0o600)
        .display()
        .to_string();
    let cloud = ElasticFake::new(
        FakeClock::new(1_000_000_000_000),
        Billing::Lambda,
        vec![offer()],
    );
    let entry = (ProviderId::known(id), cloud.clone());
    CLOUDS.lock().unwrap().insert(path.clone(), entry);
    (path, cloud)
}

/// Starts a server with the live pool `burst` of [`file_text`], its provider table replaced
/// by the platform `ours`, which lists `rack` (static), `first` and `second`, with one job
/// queued for an a100 from the start. Nothing has ticked.
fn started(name: &str) -> Started {
    let scratch = Scratch::new(name);
    let (first_key, first) = cloud(&scratch, "key-first", "first");
    let (second_key, second) = cloud(&scratch, "key-second", "second");
    let text = file_text(&scratch);
    let at = text.find("provider = {").unwrap();
    let tables = format!(
        "platforms = [\"ours\"]\n\n\
         [[machine_provider]]\nname = \"rack\"\nkind = \"static\"\n\n\
         [[machine_provider]]\nname = \"first\"\nkind = \"named-cloud\"\napi_key_file = {first_key:?}\n\n\
         [[machine_provider]]\nname = \"second\"\nkind = \"named-cloud\"\napi_key_file = {second_key:?}\n\n\
         [[platform]]\nname = \"ours\"\nkind = \"default\"\nmachine_providers = [\"rack\", \"first\", \"second\"]\n"
    );
    let text = format!("{}{tables}", &text[..at]);
    let path = scratch.write("js.toml", &text, 0o644);
    queue_job(&scratch, "wants-a100");
    let (clock, shared) = clock();
    let kinds = Kinds::standard().with("named-cloud", named_cloud);
    let config = Config::from_config_file(&path, &kinds, &OwnerOnlyFiles, shared.clone()).unwrap();
    let config = config.with_monotonic(Arc::new(Stepping::of(Duration::from_millis(1))));
    let server = InProcess::start(config, shared).unwrap();
    Started {
        server,
        clock,
        first,
        second,
        _scratch: scratch,
    }
}

/// How many acquires each cloud was asked, first then second.
fn asked(s: &Started) -> (usize, usize) {
    (s.first.count(Call::Acquire), s.second.count(Call::Acquire))
}

/// Ticks the server `seconds` later.
async fn tick_after(s: &Started, seconds: u64) {
    s.clock.advance(Duration::from_secs(seconds));
    s.server.tick().await;
}

/// Asserts the walk end to end, a tick every 5 s: the first tick asks `first` for the job's
/// machine and `second` nothing; the tick that takes in `first`'s `capacity` refusal still
/// holds the job for the refused machine, and the tick after it asks `second`, with `first`
/// asked no more; once `second` has refused too, the attempt ends at the tick after the one
/// that took its refusal in (20 s), and a tick 59 s after that asks neither, while the tick
/// at `acquire_retry` (60 s) asks `first` again, which launches the machine while `second`
/// has launched none; the pool shows no brake and no cool-down, and the record names
/// `first`. Catches a pool that launches with its first provider alone, a refusal that
/// does not fall through to the next provider, an acquire sent to the first provider
/// whatever the pick named, and a first provider that is never asked again.
#[tokio::test]
async fn a_capacity_refusal_falls_through_to_the_second_provider_and_the_first_recovers() {
    let s = started("walk-capacity");
    let capacity = [Step::Refuse(RefusalCode::Capacity)];
    s.first.script(Call::Acquire, &capacity);
    s.second.script(Call::Acquire, &capacity);
    s.server.tick().await;
    assert_eq!(asked(&s), (1, 0));
    tick_after(&s, 5).await;
    assert_eq!(asked(&s), (1, 0));
    tick_after(&s, 5).await;
    assert_eq!(asked(&s), (1, 1));
    tick_after(&s, 5).await;
    tick_after(&s, 5).await;
    assert_eq!(asked(&s), (1, 1));

    tick_after(&s, 59).await;
    assert_eq!(asked(&s), (1, 1));
    tick_after(&s, 1).await;
    assert_eq!(asked(&s), (2, 1));
    assert_eq!((s.first.launched(), s.second.launched()), (1, 0));
    let records = s.server.scaler().unwrap().state().records;
    assert!(records.cooldowns.is_empty(), "{:?}", records.cooldowns);
    assert!(records.brake.is_empty(), "{:?}", records.brake);
    let launched = records.instances.iter().map(|each| each.provider.clone());
    assert_eq!(launched.collect::<Vec<_>>(), [Some("first".to_owned())]);
}

/// Asserts that a job the first provider refuses for capacity is launched by the second,
/// the record naming `second`, and that the machine the second cloud launched is the one the
/// scaler lists and keeps: later ticks launch nothing more and the record is `Booting`.
/// Catches a machine of a later provider that the shell lists from the first provider only,
/// which would read it gone and launch again.
#[tokio::test]
async fn a_machine_of_the_second_provider_is_listed_and_kept() {
    let s = started("walk-second");
    s.first
        .script(Call::Acquire, &[Step::Refuse(RefusalCode::Capacity)]);
    s.server.tick().await;
    tick_after(&s, 5).await;
    tick_after(&s, 5).await;
    assert_eq!((s.first.launched(), s.second.launched()), (0, 1));
    for _ in 0..3 {
        tick_after(&s, 5).await;
    }
    assert_eq!(asked(&s), (1, 1));
    let records = s.server.scaler().unwrap().state().records;
    let named: Vec<_> = records
        .instances
        .iter()
        .map(|each| each.provider.clone())
        .collect();
    assert_eq!(named, [Some("second".to_owned())]);
    assert_eq!(records.instances[0].phase, js_scaler::Phase::Booting);
}
