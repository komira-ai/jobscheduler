//! The cache of the launching providers' catalogs (`js_lease_server::Catalogs`): when it
//! fetches, what it keeps when a fetch fails, how a refusal for capacity or quota makes it
//! fetch again, how an offer cools down, that it never calls the provider with its lock
//! held, and, through `POST /v1/leases`, that admission reads it and never the provider.
//! Time is an explicit `Instant` or the harness's manual clock; nothing sleeps.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use js_elastic::{
    AcquireId, AcquireRequest, Acquired, Artifact, BootStep, Capacity, CapacityProvider,
    CatalogFuture, Cooldowns, Held, Hello, HostName, HostRef, Instant, Observed, Offer, OfferKey,
    PriceUnit, ProviderCaps, ProviderError, Reason, Refusal, RefusalCode, Released, SchedulerId,
    Secret,
};
use js_lease_server::network::{Contribution, LaunchCtx, NetworkError, NetworkMode};
use js_lease_server::{
    BuildCtx, Built, Catalogs, Config, ElasticBuilt, Floor, KindError, Launcher, OwnerOnlyFiles,
};
use js_scaler::AcquirePlan;
use js_sched::admission_ever::CatalogView;
use js_types::{ExecMode, Model, ProviderId, accelerator_of};
use serde_json::{Value, json};
use tokio::sync::Notify;

use crate::agents_support::JOIN;
use crate::config_support::{PoolText, file, kinds, quote, quoted, server};
use crate::support::{Harness, LogCapture, Reply, Scratch};

/// One hour, the default of both `catalog_refresh` and `offer_cooldown`.
const HOUR: Duration = Duration::from_secs(3600);

/// One day: a refresh interval no test here reaches.
const DAY: Duration = Duration::from_secs(24 * 3600);

/// Milliseconds in an hour.
const HOUR_MS: u64 = 3_600_000;

/// The text every warning of a failed catalog read starts with.
const WARNING: &str = "cannot read the provider's catalog";

/// The instant `ms` milliseconds after `at`.
fn after(at: Instant, ms: u64) -> Instant {
    Instant::from_millis(at.millis() + ms)
}

/// The origin of the tests' explicit clock.
fn t0() -> Instant {
    Instant::from_millis(10_000)
}

/// An offer of the type `id` in `zone`: one `nvidia-a100` of a Linux x86-64 machine.
fn offer(id: &str, zone: Option<&str>) -> Offer {
    Offer {
        id: id.to_owned(),
        zone: zone.map(str::to_owned),
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
        price_cents_per_hour: Some(129),
        price_unit: PriceUnit::UsCents,
        available: true,
        purchase: None,
        quota: None,
        block: None,
    }
}

/// The catalog the tests start with.
fn list_a() -> Vec<Offer> {
    vec![offer("type-a", Some("z1"))]
}

/// The catalog a refresh replaces [`list_a`] with.
fn list_b() -> Vec<Offer> {
    vec![offer("type-b", Some("z1"))]
}

/// How admission sees `list`.
fn offers(list: &[Offer]) -> CatalogView {
    CatalogView::Offers(list.to_vec())
}

/// A gate a catalog call waits at: it says it has entered, then waits to be let go.
struct Gate {
    entered: Notify,
    release: Notify,
}

/// A launching provider whose catalog the test scripts and whose catalog calls it counts at
/// the time of the call, and whose acquire answers what the test set.
struct Counting {
    id: ProviderId,
    catalog: Mutex<Result<Vec<Offer>, String>>,
    calls: AtomicUsize,
    answer: Mutex<Acquired>,
    gate: Option<Arc<Gate>>,
    panic_next: AtomicBool,
}

impl Counting {
    fn build(id: &'static str, gate: Option<Arc<Gate>>) -> Arc<Self> {
        Arc::new(Self {
            id: ProviderId::known(id),
            catalog: Mutex::new(Ok(Vec::new())),
            calls: AtomicUsize::new(0),
            answer: Mutex::new(Acquired::Unknown),
            gate,
            panic_next: AtomicBool::new(false),
        })
    }

    /// A provider `id` whose catalog is empty until set.
    fn new(id: &'static str) -> Arc<Self> {
        Self::build(id, None)
    }

    /// A provider `id` whose every catalog call waits at `gate`.
    fn gated(id: &'static str, gate: Arc<Gate>) -> Arc<Self> {
        Self::build(id, Some(gate))
    }

    /// The answer of the next catalog calls.
    fn set(&self, catalog: Result<Vec<Offer>, &str>) {
        *self.catalog.lock().unwrap() = catalog.map_err(str::to_owned);
    }

    /// Makes every acquire answer a refusal with `code`.
    fn refuse(&self, code: RefusalCode) {
        *self.answer.lock().unwrap() = Acquired::Refused(Refusal {
            code,
            reason: Reason::Api("refused".to_owned()),
        });
    }

    /// How many catalog calls were made.
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn id(&self) -> ProviderId {
        self.id.clone()
    }

    /// This provider as the trait object the caches and the launcher take.
    fn provider(self: &Arc<Self>) -> Arc<dyn CapacityProvider> {
        Arc::new(Handle(self.clone()))
    }

    /// Catalogs of this provider alone.
    fn catalogs(self: &Arc<Self>, refresh: Duration, cooldown: Duration) -> Catalogs {
        Catalogs::new().with(self.provider(), refresh, cooldown)
    }
}

/// The provider a cache or a pool holds: the test's [`Counting`].
struct Handle(Arc<Counting>);

impl CapacityProvider for Handle {
    fn id(&self) -> ProviderId {
        self.0.id()
    }

    fn can_launch(&self) -> bool {
        true
    }

    fn capabilities(&self) -> ProviderCaps {
        ProviderCaps {
            os: BTreeSet::from(["linux".to_owned()]),
            arch: BTreeSet::from(["x86_64".to_owned()]),
            accelerators: BTreeSet::from([Model::parse("nvidia-a100").unwrap()]),
            whole_host: true,
            host_ports: false,
        }
    }

    fn catalog(&self) -> CatalogFuture<'_> {
        self.0.calls.fetch_add(1, Ordering::SeqCst);
        if self.0.panic_next.swap(false, Ordering::SeqCst) {
            panic!("the catalog call was told to panic");
        }
        let answer = self
            .0
            .catalog
            .lock()
            .unwrap()
            .clone()
            .map_err(ProviderError::new);
        let gate = self.0.gate.clone();
        Box::pin(async move {
            if let Some(gate) = gate {
                gate.entered.notify_one();
                gate.release.notified().await;
            }
            answer
        })
    }

    fn capacity(&self) -> Capacity {
        Capacity {
            supplied: 0,
            can_add: None,
        }
    }

    fn host_prep(&self, _offer: &Offer) -> Vec<BootStep> {
        Vec::new()
    }

    fn payload_limit(&self) -> usize {
        1 << 20
    }

    fn acquire(&self, _request: &AcquireRequest) -> Acquired {
        self.0.answer.lock().unwrap().clone()
    }

    fn list(&self) -> Result<Vec<Held>, ProviderError> {
        Ok(Vec::new())
    }

    fn release(&self, _id: &HostRef) -> Released {
        Released::Unknown
    }

    fn observe(&self, _id: &HostRef) -> Observed {
        Observed::Unknown
    }

    fn verify_instance(&self, _hello: &Hello) -> Option<HostRef> {
        None
    }
}

/// A network mode that adds nothing to a launch.
struct NoNetwork;

impl NetworkMode for NoNetwork {
    fn prepare(&self, _launch: &LaunchCtx<'_>) -> Result<Contribution, NetworkError> {
        Ok(Contribution::empty())
    }

    fn describe(&self, _launch: &LaunchCtx<'_>) -> Vec<BootStep> {
        Vec::new()
    }

    fn check(&self) -> Result<(), NetworkError> {
        Ok(())
    }

    fn forget(&self, _host: &HostName) -> Result<(), NetworkError> {
        Ok(())
    }
}

/// A launcher over `cloud` that hands its provider's answers to `catalogs`.
fn launcher(cloud: &Arc<Counting>, catalogs: &Arc<Catalogs>) -> Launcher {
    let agents = BTreeMap::from([(
        "x86_64".to_owned(),
        Artifact {
            url: "https://sched.example.org:7301/agent".to_owned(),
            sha256: [7; 32],
        },
    )]);
    Launcher::new(
        cloud.provider(),
        Arc::new(NoNetwork),
        SchedulerId::new("sched").unwrap(),
        "https://sched.example.org:7301".to_owned(),
        agents,
    )
    .with_catalogs(catalogs.clone())
}

/// The plan of one acquire of the pool `burst`, of an offer of [`list_a`].
fn plan() -> AcquirePlan {
    let acquire = AcquireId::new("burst-0001").unwrap();
    let name = HostName::derive(&SchedulerId::new("sched").unwrap(), &acquire);
    AcquirePlan {
        name: name.to_string(),
        acquire,
        attempt: 0,
        pool: "burst".to_owned(),
        offer: list_a().remove(0),
        lifetime: None,
    }
}

/// Asserts that the cache fetches a provider's catalog on the first call, not again just
/// before `catalog_refresh` has passed since the fetch, and again at exactly that time,
/// where the view shows the new list. Catches a cache that fetches at each call, one that
/// never refetches, and an interval off by a tick.
#[tokio::test]
async fn the_cache_refreshes_on_the_interval() {
    let cloud = Counting::new("interval-cloud");
    cloud.set(Ok(list_a()));
    let interval = Duration::from_secs(600);
    let catalogs = cloud.catalogs(interval, HOUR);
    let id = cloud.id();
    catalogs.refresh_due(t0()).await;
    assert_eq!(cloud.calls(), 1);
    assert_eq!(catalogs.view(&id, t0()), offers(&list_a()));
    cloud.set(Ok(list_b()));
    let just_before = after(t0(), 600_000 - 1);
    catalogs.refresh_due(just_before).await;
    assert_eq!(cloud.calls(), 1);
    assert_eq!(catalogs.view(&id, just_before), offers(&list_a()));
    let due = t0().saturating_add(interval);
    catalogs.refresh_due(due).await;
    assert_eq!(cloud.calls(), 2);
    assert_eq!(catalogs.view(&id, due), offers(&list_b()));
}

/// Asserts that a fetch that fails keeps the last good catalog in view (offers, not
/// unread), is tried again at the very next call and not after another interval, warns once
/// for an outage however many fetches fail in it, and warns again for a failure after a
/// fetch that worked. Catches a failure that empties or unreads the catalog, one that waits
/// out the interval, and a warning at each failed tick.
#[tokio::test]
async fn a_failed_refresh_keeps_the_last_good_catalog() {
    let logs = LogCapture::start();
    let cloud = Counting::new("failing-cloud");
    cloud.set(Ok(list_a()));
    let catalogs = cloud.catalogs(HOUR, HOUR);
    let id = cloud.id();
    catalogs.refresh_due(t0()).await;
    assert_eq!(cloud.calls(), 1);
    assert_eq!(logs.text().matches(WARNING).count(), 0);

    cloud.set(Err("the cloud is down"));
    let due = after(t0(), HOUR_MS);
    catalogs.refresh_due(due).await;
    assert_eq!(cloud.calls(), 2);
    assert_eq!(catalogs.view(&id, due), offers(&list_a()));
    assert_eq!(logs.text().matches(WARNING).count(), 1);

    let next = after(due, 1);
    catalogs.refresh_due(next).await;
    assert_eq!(cloud.calls(), 3);
    assert_eq!(catalogs.view(&id, next), offers(&list_a()));
    assert_eq!(logs.text().matches(WARNING).count(), 1);
    assert!(logs.text().contains("the cloud is down"), "{}", logs.text());

    cloud.set(Ok(list_b()));
    let recovered = after(due, 2);
    catalogs.refresh_due(recovered).await;
    assert_eq!(cloud.calls(), 4);
    assert_eq!(catalogs.view(&id, recovered), offers(&list_b()));

    cloud.set(Err("the cloud is down again"));
    let again = after(recovered, HOUR_MS);
    catalogs.refresh_due(again).await;
    assert_eq!(cloud.calls(), 5);
    assert_eq!(catalogs.view(&id, again), offers(&list_b()));
    assert_eq!(logs.text().matches(WARNING).count(), 2);
}

/// Asserts that `Catalogs::fetches` counts the fetches that completed: zero before any, one
/// after a refresh that fetched, unchanged by a refresh that was not due, unchanged by a
/// fetch that failed (the provider was called all the same), one more after the fetch an
/// invalidation by a capacity refusal made due, and zero for a provider the catalogs do
/// not hold. It catches a count that moves on a failed fetch or on a refresh that fetched
/// nothing, one that misses a fetch an invalidation caused, and one that reads another
/// provider's count.
#[tokio::test]
async fn a_completed_fetch_is_counted_and_a_failed_one_is_not() {
    let cloud = Counting::new("counted-cloud");
    cloud.set(Ok(list_a()));
    let catalogs = Arc::new(cloud.catalogs(HOUR, HOUR));
    let id = cloud.id();
    assert_eq!(catalogs.fetches(&id), 0);

    catalogs.refresh_due(t0()).await;
    assert_eq!((cloud.calls(), catalogs.fetches(&id)), (1, 1));

    catalogs.refresh_due(after(t0(), 1)).await;
    assert_eq!((cloud.calls(), catalogs.fetches(&id)), (1, 1));

    cloud.set(Err("the cloud is down"));
    catalogs.refresh_due(after(t0(), HOUR_MS)).await;
    assert_eq!((cloud.calls(), catalogs.fetches(&id)), (2, 1));

    cloud.set(Ok(list_b()));
    catalogs.refresh_due(after(t0(), HOUR_MS + 1)).await;
    assert_eq!((cloud.calls(), catalogs.fetches(&id)), (3, 2));

    cloud.refuse(RefusalCode::Capacity);
    let launcher = launcher(&cloud, &catalogs);
    let answer = launcher.acquire(&plan(), Secret::new("join-token".to_owned()));
    assert!(matches!(answer, Acquired::Refused(_)));
    assert_eq!(catalogs.fetches(&id), 2);
    catalogs.refresh_due(after(t0(), HOUR_MS + 2)).await;
    assert_eq!((cloud.calls(), catalogs.fetches(&id)), (4, 3));

    assert_eq!(catalogs.fetches(&ProviderId::known("stranger-cloud")), 0);
}

/// Asserts that `Catalogs::fetched` names no fetch before the first, then the instant of
/// the fetch that worked and the count of its offers; that a failed fetch after it moves
/// neither; that reading it calls no provider; and that a provider the catalogs do not hold
/// has none. Catches a fetch time taken from a failed attempt, a read that calls the
/// provider, and another provider's catalog read.
#[tokio::test]
async fn the_fetch_time_and_offer_count_are_read_without_a_call() {
    let cloud = Counting::new("fetched-cloud");
    cloud.set(Ok(list_a()));
    let catalogs = cloud.catalogs(HOUR, HOUR);
    let id = cloud.id();
    assert_eq!(catalogs.fetched(&id), None);
    catalogs.refresh_due(t0()).await;
    assert_eq!(catalogs.fetched(&id), Some((t0(), list_a().len())));
    cloud.set(Err("the cloud is down"));
    catalogs.refresh_due(after(t0(), HOUR_MS)).await;
    assert_eq!(catalogs.fetched(&id), Some((t0(), list_a().len())));
    assert_eq!(cloud.calls(), 2);
    assert_eq!(catalogs.fetched(&ProviderId::known("stranger-cloud")), None);
}

/// The `cache-cloud` clouds of these tests, by the path of the key file that names them: a
/// kind is a plain function, so a test leaves its cloud here for the kind to find.
static CLOUDS: Mutex<BTreeMap<String, Arc<Counting>>> = Mutex::new(BTreeMap::new());

/// The `cache-cloud` kind: a pool over the cloud the test left for its key file, with one
/// `nvidia-a100` GPU as its floor.
fn cache_cloud(table: &toml::Table, _ctx: &BuildCtx<'_>) -> Result<Built, KindError> {
    let path = table.get("api_key_file").and_then(toml::Value::as_str);
    let cloud = CLOUDS
        .lock()
        .unwrap()
        .get(path.unwrap_or_default())
        .cloned();
    Ok(Built::Elastic(ElasticBuilt {
        idle_costs: false,
        provider: Box::new(Handle(cloud.unwrap())),
        floor: Floor {
            gpus: 1,
            gpu_class: Some(Model::parse("nvidia-a100").unwrap()),
        },
        exec: vec![ExecMode::Container],
    }))
}

/// A server whose one pool, `burst`, of class `gpu-host` launches on a fresh cloud; the
/// cloud with it. Its catalog is empty until set.
async fn started(name: &str) -> (Harness, Arc<Counting>) {
    let scratch = Scratch::new(name);
    let cloud = Counting::new("cache-cloud");
    let key = scratch.write("cloud-key", "unused", 0o600);
    CLOUDS
        .lock()
        .unwrap()
        .insert(key.display().to_string(), cloud.clone());
    let joins = scratch.write("joins", &format!("{JOIN}\n"), 0o600);
    let provider = format!(
        "{{ kind = \"cache-cloud\", api_key_file = {} }}",
        quoted(&key)
    );
    let pool = PoolText::elastic(&scratch)
        .set("name", &quote("burst"))
        .set("class", &quote("gpu-host"))
        .set("exec", &quote("none"))
        .set("provider", &provider)
        .set("join_token_file", &quoted(&joins));
    let head = server(
        &scratch,
        "listen = \"127.0.0.1:0\"\nagent_listen = \"127.0.0.2:0\"",
    );
    let path = scratch.write("js.toml", &file(&head, &[&pool]), 0o644);
    let kinds = kinds().with("cache-cloud", cache_cloud);
    let clock = crate::config_support::clock();
    let config = Config::from_config_file(&path, &kinds, &OwnerOnlyFiles, clock).unwrap();
    (Harness::over(scratch, config).await, cloud)
}

/// A request for one `nvidia-a100` of the class `gpu-host`, which only a catalog could
/// serve.
fn one_gpu() -> Value {
    json!({
        "class": "gpu-host",
        "resources": { "cpu_millis": 1000, "memory_bytes": 1024, "gpus": 1, "gpu_class": "nvidia-a100" },
        "max_wall_seconds": 600,
    })
}

async fn post(h: &Harness) -> Reply {
    h.call("POST", "/v1/leases", Some(&one_gpu())).await
}

/// Asserts that `reply` is a 409 with the error `code`.
fn refused(reply: &Reply, code: &str) {
    assert_eq!(reply.status, 409, "{}", reply.body);
    assert_eq!(reply.json(), json!({ "error": code }));
}

/// Asserts, at the level of the cache and then through the route, that a catalog never
/// fetched, and one whose first fetch failed, read as unread ("may offer"), while one that
/// was fetched and is empty reads as an empty list. Through `POST /v1/leases`: before any
/// tick a request only a catalog could serve is admitted (the start answers `no-capacity`)
/// and the provider is not called; after a tick whose fetch failed it still is, and the
/// fetch is tried again at the next tick; a tick that fetches an empty list makes the same
/// request `no-capable-host`; that list is kept until the clock moves past
/// `catalog_refresh`, after which a tick fetches the new one and the request is admitted
/// again. Catches an unfetched or failed catalog read as an empty one, which would refuse
/// work a cloud could serve.
#[tokio::test]
async fn a_never_fetched_catalog_is_may_offer() {
    let cloud = Counting::new("unread-cloud");
    cloud.set(Err("the cloud is down"));
    let catalogs = cloud.catalogs(HOUR, HOUR);
    let id = cloud.id();
    assert_eq!(catalogs.view(&id, t0()), CatalogView::Unread);
    catalogs.refresh_due(t0()).await;
    assert_eq!(cloud.calls(), 1);
    assert_eq!(catalogs.view(&id, t0()), CatalogView::Unread);
    cloud.set(Ok(Vec::new()));
    catalogs.refresh_due(after(t0(), 1)).await;
    assert_eq!(cloud.calls(), 2);
    assert_eq!(
        catalogs.view(&id, after(t0(), 1)),
        CatalogView::Offers(vec![])
    );

    let (h, cloud) = started("cache_never_fetched").await;
    cloud.set(Err("the cloud's API did not answer"));
    refused(&post(&h).await, "no-capacity");
    assert_eq!(cloud.calls(), 0);
    h.ticks.step().await;
    assert_eq!(cloud.calls(), 1);
    refused(&post(&h).await, "no-capacity");
    cloud.set(Ok(Vec::new()));
    h.ticks.step().await;
    assert_eq!(cloud.calls(), 2);
    refused(&post(&h).await, "no-capable-host");
    cloud.set(Ok(vec![offer("gpu_1x_a100", Some("zone-a"))]));
    h.ticks.step().await;
    assert_eq!(cloud.calls(), 2);
    refused(&post(&h).await, "no-capable-host");
    h.clock.advance(Duration::from_secs(2 * 3600));
    h.ticks.step().await;
    assert_eq!(cloud.calls(), 3);
    refused(&post(&h).await, "no-capacity");
    assert_eq!(cloud.calls(), 3);
}

/// Launches through the real launcher one machine of the provider that answers a refusal
/// with `code`, after a catalog fetched at the origin. Asserts the offers stay readable
/// after the refusal, and that the next call to refresh is due (and fetches, once) exactly
/// when `invalidates`, and not again after that.
async fn refusal_case(code: RefusalCode, invalidates: bool) {
    let cloud = Counting::new("refusal-cloud");
    cloud.set(Ok(list_a()));
    let catalogs = Arc::new(cloud.catalogs(HOUR, HOUR));
    let id = cloud.id();
    catalogs.refresh_due(t0()).await;
    assert_eq!(cloud.calls(), 1);

    cloud.refuse(code);
    let launcher = launcher(&cloud, &catalogs);
    let answer = launcher.acquire(&plan(), Secret::new("join-token".to_owned()));
    let refusal = Refusal {
        code,
        reason: Reason::Api("refused".to_owned()),
    };
    assert_eq!(answer, Acquired::Refused(refusal));

    let next = after(t0(), 1);
    assert_eq!(catalogs.view(&id, next), offers(&list_a()));
    catalogs.refresh_due(next).await;
    if invalidates {
        assert_eq!(cloud.calls(), 2);
        catalogs.refresh_due(after(t0(), 2)).await;
    }
    assert_eq!(cloud.calls(), if invalidates { 2 } else { 1 });
    assert_eq!(catalogs.view(&id, after(t0(), 2)), offers(&list_a()));
}

/// Asserts, through the launcher's real acquire, that a provider's refusal for capacity
/// makes the next refresh fetch the catalog again although the interval has not passed,
/// that the stale offers are readable until it has, and that the one fetch clears the
/// invalidation. Catches an offer list that outlives the capacity it described.
#[tokio::test]
async fn a_capacity_refusal_invalidates_the_cache() {
    refusal_case(RefusalCode::Capacity, true).await;
}

/// Asserts the same as for capacity, for a refusal for quota. Catches a quota refusal read
/// as a fact about the request and not about the account's catalog.
#[tokio::test]
async fn a_quota_refusal_invalidates_the_cache() {
    refusal_case(RefusalCode::Quota, true).await;
}

/// Asserts that a refusal for shape (the request does not fit what the provider knows)
/// leaves the cache alone: the next refresh makes no call. Catches a refusal about the
/// request that makes every launch refetch the catalog.
#[tokio::test]
async fn a_shape_refusal_does_not_invalidate_the_cache() {
    refusal_case(RefusalCode::Shape, false).await;
}

/// Asserts that a refusal for denial (the key is refused) leaves the cache alone: the next
/// refresh makes no call. Catches a refusal about the key that makes every launch refetch
/// the catalog.
#[tokio::test]
async fn a_denied_refusal_does_not_invalidate_the_cache() {
    refusal_case(RefusalCode::Denied, false).await;
}

/// Asserts that a refusal for capacity reported for a provider id the catalogs do not hold
/// invalidates none of the caches they do hold. Catches a refusal that invalidates every
/// cache, or one that is keyed by the wrong id.
#[tokio::test]
async fn a_refusal_of_an_unknown_provider_invalidates_nothing() {
    let cloud = Counting::new("held-cloud");
    cloud.set(Ok(list_a()));
    let catalogs = cloud.catalogs(HOUR, HOUR);
    catalogs.refresh_due(t0()).await;
    let stranger = ProviderId::known("stranger-cloud");
    let refusal = Refusal {
        code: RefusalCode::Capacity,
        reason: Reason::Api("refused".to_owned()),
    };
    catalogs.acquired(&stranger, &Acquired::Refused(refusal));
    catalogs.refresh_due(after(t0(), 1)).await;
    assert_eq!(cloud.calls(), 1);
}

/// Asserts that answers that are not refusals (no answer, a deferred launch) invalidate
/// nothing. Catches an answer that says nothing read as a reason to fetch.
#[tokio::test]
async fn an_answer_that_is_not_a_refusal_invalidates_nothing() {
    let cloud = Counting::new("silent-cloud");
    cloud.set(Ok(list_a()));
    let catalogs = cloud.catalogs(HOUR, HOUR);
    let id = cloud.id();
    catalogs.refresh_due(t0()).await;
    catalogs.acquired(&id, &Acquired::Unknown);
    let wait = Duration::from_secs(30);
    catalogs.acquired(&id, &Acquired::Deferred { wait });
    catalogs.refresh_due(after(t0(), 1)).await;
    assert_eq!(cloud.calls(), 1);
}

/// Asserts that an offer cooled down at the origin is left out of the offers pass 2 may
/// pick from until its provider's `offer_cooldown` has passed (one millisecond short of it
/// it is still out, at it the offer is back), while the view admission reads holds it
/// throughout. Catches a cool-down that ends early or late, and one that hides the offer
/// from admission, which would refuse work the offer can serve once it cools off.
#[tokio::test]
async fn a_cooled_down_offer_is_not_picked_until_the_cooldown_ends() {
    let cloud = Counting::new("cooling-cloud");
    let (one, two) = (offer("type-1", Some("z1")), offer("type-2", Some("z1")));
    cloud.set(Ok(vec![one.clone(), two.clone()]));
    let catalogs = cloud.catalogs(DAY, HOUR);
    let id = cloud.id();
    catalogs.refresh_due(t0()).await;
    catalogs.cool_down(OfferKey::of(&id, &one), t0());
    let whole = offers(&[one.clone(), two.clone()]);
    for at in [t0(), after(t0(), HOUR_MS - 1)] {
        assert_eq!(catalogs.offers_for_pass2(&id, at), vec![two.clone()]);
        assert_eq!(catalogs.view(&id, at), whole);
    }
    let ended = after(t0(), HOUR_MS);
    assert_eq!(catalogs.offers_for_pass2(&id, ended), vec![one, two]);
    assert_eq!(catalogs.view(&id, ended), whole);
}

/// Asserts that a cool-down belongs to one provider, one instance type and one zone: of
/// two providers offering the same three offers, cooling down one type in one zone of the
/// first leaves that type in the other zone, the other type, and all of the second
/// provider's offers in. Catches a key that leaves out the provider, the zone or the type.
#[tokio::test]
async fn a_cooldown_is_keyed_by_provider_instance_type_and_zone() {
    let first = Counting::new("keyed-a");
    let second = Counting::new("keyed-b");
    let cooled = offer("t1", Some("z1"));
    let other_zone = offer("t1", Some("z2"));
    let other_type = offer("t2", Some("z1"));
    let list = vec![cooled.clone(), other_zone.clone(), other_type.clone()];
    first.set(Ok(list.clone()));
    second.set(Ok(list.clone()));
    let catalogs =
        Catalogs::new()
            .with(first.provider(), DAY, HOUR)
            .with(second.provider(), DAY, HOUR);
    catalogs.refresh_due(t0()).await;
    assert_eq!((first.calls(), second.calls()), (1, 1));
    catalogs.cool_down(OfferKey::of(&first.id(), &cooled), t0());
    assert_eq!(
        catalogs.offers_for_pass2(&first.id(), t0()),
        vec![other_zone, other_type]
    );
    assert_eq!(catalogs.offers_for_pass2(&second.id(), t0()), list);
}

/// Asserts that cooling an offer down again never shortens the cool-down it has: one
/// started half an hour in, then one started at the origin, still ends the offer's
/// cool-down at an hour and a half. Catches a later call, however early its own end, that
/// replaces the end instead of keeping the later one.
#[tokio::test]
async fn a_second_cooldown_never_shortens_the_first() {
    let cloud = Counting::new("lengthening-cloud");
    let (one, two) = (offer("type-1", Some("z1")), offer("type-2", Some("z1")));
    cloud.set(Ok(vec![one.clone(), two.clone()]));
    let catalogs = cloud.catalogs(DAY, HOUR);
    let id = cloud.id();
    catalogs.refresh_due(t0()).await;
    let key = OfferKey::of(&id, &one);
    catalogs.cool_down(key.clone(), after(t0(), HOUR_MS / 2));
    catalogs.cool_down(key, t0());
    let still = after(t0(), HOUR_MS + HOUR_MS / 2 - 1);
    assert_eq!(catalogs.offers_for_pass2(&id, still), vec![two.clone()]);
    let ended = after(t0(), HOUR_MS + HOUR_MS / 2);
    assert_eq!(catalogs.offers_for_pass2(&id, ended), vec![one, two]);
}

/// Asserts that cooling an offer down forgets only the cool-downs that have ended: with a
/// long cool-down (an hour) and a short one (ten minutes) started at the origin, cooling a
/// third offer down once the short one has ended leaves the long one in force until its own
/// end, the short one's offer back in, and the third offer out. Catches a forgetting that
/// drops a cool-down still running, which would put an offer back too soon.
#[tokio::test]
async fn forget_expired_drops_only_ended_cooldowns() {
    let long = Counting::new("long-cloud");
    let short = Counting::new("short-cloud");
    let (one, two) = (offer("type-1", Some("z1")), offer("type-2", Some("z1")));
    long.set(Ok(vec![one.clone(), two.clone()]));
    short.set(Ok(vec![one.clone(), two.clone()]));
    let catalogs = Catalogs::new().with(long.provider(), DAY, HOUR).with(
        short.provider(),
        DAY,
        Duration::from_secs(600),
    );
    catalogs.refresh_due(t0()).await;
    catalogs.cool_down(OfferKey::of(&long.id(), &one), t0());
    catalogs.cool_down(OfferKey::of(&short.id(), &one), t0());
    let ten_minutes = after(t0(), 600_000);
    catalogs.cool_down(OfferKey::of(&long.id(), &two), ten_minutes);
    assert!(
        catalogs
            .offers_for_pass2(&long.id(), ten_minutes)
            .is_empty()
    );
    assert_eq!(
        catalogs.offers_for_pass2(&short.id(), ten_minutes),
        vec![one.clone(), two.clone()]
    );
    let almost = after(t0(), HOUR_MS - 1);
    assert!(catalogs.offers_for_pass2(&long.id(), almost).is_empty());
    let ended = after(t0(), HOUR_MS);
    assert_eq!(catalogs.offers_for_pass2(&long.id(), ended), vec![one]);
}

/// Asserts that a catalog that was fetched and is empty reads as an empty list (no offers,
/// none for pass 2) and is kept as that until the interval has passed, where one never
/// fetched reads as unread. Catches an empty catalog taken for no catalog, which would
/// admit work no machine could serve.
#[tokio::test]
async fn an_empty_catalog_that_was_read_is_not_never() {
    let cloud = Counting::new("empty-cloud");
    let catalogs = cloud.catalogs(HOUR, HOUR);
    let id = cloud.id();
    assert_eq!(catalogs.view(&id, t0()), CatalogView::Unread);
    catalogs.refresh_due(t0()).await;
    assert_eq!(cloud.calls(), 1);
    assert_eq!(catalogs.view(&id, t0()), CatalogView::Offers(vec![]));
    assert!(catalogs.offers_for_pass2(&id, t0()).is_empty());
    catalogs.refresh_due(after(t0(), HOUR_MS - 1)).await;
    assert_eq!(cloud.calls(), 1);
}

/// Asserts that a provider id the catalogs do not hold reads as unread, offers pass 2
/// nothing, keeps no cool-down (an offer of a held provider is not hidden by it) and
/// invalidates nothing, and that catalogs with no provider refresh nothing. Catches a read
/// of an unknown provider that panics or reads as an empty catalog, which would refuse
/// every request.
#[tokio::test]
async fn a_provider_the_catalogs_do_not_hold_reads_as_unread() {
    let cloud = Counting::new("known-cloud");
    let held = offer("type-1", Some("z1"));
    cloud.set(Ok(vec![held.clone()]));
    let catalogs = cloud.catalogs(DAY, HOUR);
    catalogs.refresh_due(t0()).await;
    let stranger = ProviderId::known("stranger-cloud");
    assert_eq!(catalogs.view(&stranger, t0()), CatalogView::Unread);
    assert!(catalogs.offers_for_pass2(&stranger, t0()).is_empty());
    catalogs.cool_down(OfferKey::of(&stranger, &held), t0());
    assert_eq!(catalogs.offers_for_pass2(&cloud.id(), t0()), vec![held]);

    let none = Catalogs::new();
    none.refresh_due(t0()).await;
    assert_eq!(none.view(&cloud.id(), t0()), CatalogView::Unread);
    assert_eq!(cloud.calls(), 1);
}

/// Asserts that a second provider given under the id of the first takes its place: a
/// refresh calls only the second, and the view holds the second's catalog. Catches a
/// provider that is kept beside the one it replaces, or a replacement that is dropped.
#[tokio::test]
async fn a_second_provider_with_the_same_id_replaces_the_first() {
    let first = Counting::new("twin-cloud");
    let second = Counting::new("twin-cloud");
    first.set(Ok(list_a()));
    second.set(Ok(list_b()));
    let catalogs =
        Catalogs::new()
            .with(first.provider(), HOUR, HOUR)
            .with(second.provider(), HOUR, HOUR);
    catalogs.refresh_due(t0()).await;
    assert_eq!((first.calls(), second.calls()), (0, 1));
    assert_eq!(catalogs.view(&first.id(), t0()), offers(&list_b()));
}

/// Asserts, through the route, that admission reads the cache: after one tick has fetched
/// the catalog once, a hundred requests only the catalog could serve are each admitted (the
/// start answers `no-capacity`) and the provider's call count stays at one. Catches an
/// admission that calls the provider for each request.
#[tokio::test]
async fn admission_reads_the_cache_and_never_calls_the_provider() {
    let (h, cloud) = started("cache_admission_reads").await;
    cloud.set(Ok(vec![offer("gpu_1x_a100", Some("zone-a"))]));
    assert_eq!(cloud.calls(), 0);
    h.ticks.step().await;
    assert_eq!(cloud.calls(), 1);
    for _ in 0..100 {
        refused(&post(&h).await, "no-capacity");
    }
    assert_eq!(cloud.calls(), 1);
}

/// Asserts that the provider is not called with the cache's lock held: while a refresh is
/// inside the provider's catalog call, a read of the same catalog answers at once (unread,
/// nothing has come), and once the call is let go the refresh records the catalog and the
/// view shows it. Catches a refresh that holds the lock across the call, which would stall
/// admission (under the lease table's lock) for as long as a cloud takes to answer.
#[tokio::test]
async fn the_provider_is_not_called_while_the_cache_lock_is_held() {
    let gate = Arc::new(Gate {
        entered: Notify::new(),
        release: Notify::new(),
    });
    let cloud = Counting::gated("gated-cloud", gate.clone());
    cloud.set(Ok(list_a()));
    let id = cloud.id();
    let catalogs = Arc::new(cloud.catalogs(HOUR, HOUR));
    let refreshing = {
        let catalogs = catalogs.clone();
        tokio::spawn(async move { catalogs.refresh_due(t0()).await })
    };
    gate.entered.notified().await;
    assert_eq!(cloud.calls(), 1);
    assert_eq!(catalogs.view(&id, t0()), CatalogView::Unread);
    assert!(catalogs.offers_for_pass2(&id, t0()).is_empty());
    gate.release.notify_one();
    refreshing.await.unwrap();
    assert_eq!(catalogs.view(&id, t0()), offers(&list_a()));
}

/// Asserts that a catalog call that panics is a failed fetch: the refresh returns (the
/// caller's task, the scaler's in the server, goes on), the last good catalog stays
/// readable, and the next refresh fetches again rather than waiting another interval.
/// Catches a fetch that lets a provider's panic end the task that ticks the scaler, which
/// would stop every launch and release.
#[tokio::test]
async fn a_catalog_call_that_panics_is_a_failed_fetch() {
    let cloud = Counting::new("panicky");
    cloud.set(Ok(list_a()));
    let catalogs = cloud.catalogs(HOUR, HOUR);
    catalogs.refresh_due(t0()).await;
    assert_eq!(cloud.calls(), 1);
    cloud.panic_next.store(true, Ordering::SeqCst);
    let due = after(t0(), HOUR_MS);
    catalogs.refresh_due(due).await;
    assert_eq!(cloud.calls(), 2);
    assert_eq!(catalogs.view(&cloud.id(), due), offers(&list_a()));
    cloud.set(Ok(list_b()));
    catalogs.refresh_due(after(due, 1)).await;
    assert_eq!(cloud.calls(), 3);
    assert_eq!(catalogs.view(&cloud.id(), after(due, 1)), offers(&list_b()));
}

/// Asserts that cool-downs read back from the scaler's records and held by the catalogs
/// leave each offer out of pass 2's offers up to its own end and not past it, whatever the
/// provider's `offer_cooldown`; that a later hold with an earlier end does not bring the end
/// forward; and that a cool-down of a provider the catalogs do not hold hides none of the
/// held provider's offers. Catches a cool-down read back after a restart that is dropped,
/// moved to a fresh `offer_cooldown` from the restart, or shortened.
#[tokio::test]
async fn cooldowns_held_from_the_records_last_to_their_ends() {
    let cloud = Counting::new("held-cloud");
    let (one, two) = (offer("type-1", Some("z1")), offer("type-2", Some("z1")));
    cloud.set(Ok(vec![one.clone(), two.clone()]));
    let catalogs = cloud.catalogs(DAY, HOUR);
    let id = cloud.id();
    catalogs.refresh_due(t0()).await;
    let end = after(t0(), 3 * HOUR_MS);
    let stranger = ProviderId::known("stranger-cloud");
    let mut read = Cooldowns::new();
    read.hold_until(OfferKey::of(&id, &one), end);
    read.hold_until(OfferKey::of(&stranger, &two), end);
    catalogs.hold(&read);
    let mut earlier = Cooldowns::new();
    earlier.hold_until(OfferKey::of(&id, &one), after(t0(), 10));
    catalogs.hold(&earlier);
    for at in [t0(), after(t0(), 3 * HOUR_MS - 1)] {
        assert_eq!(catalogs.offers_for_pass2(&id, at), vec![two.clone()]);
    }
    assert_eq!(catalogs.offers_for_pass2(&id, end), vec![one, two]);
}
