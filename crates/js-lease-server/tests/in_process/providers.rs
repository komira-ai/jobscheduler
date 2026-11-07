//! `ListProviders` on the job port of an in-process server over the elastic fake cloud: what
//! it tells of the provider besides its checks and machines (the catalog, the instance types
//! passed over for quota, the brake, the cool-down and the launches out), read through the
//! gateway (`GET /v1/providers`) as an operator reads it. The cloud's acquires are scripted
//! or held at a gate the test opens; time is the test's clock, and nothing sleeps.

use std::collections::{BTreeMap, VecDeque};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use js_elastic::{
    AcquireId, AcquireRequest, Acquired, BootStep, CapacityProvider, CatalogFuture, CheckResult,
    Held, HostRef, Offer, ProviderError, Reason, Refusal, RefusalCode, Released,
};
use js_elastic_fake::{Billing, ElasticFake, FakeClock};
use js_lease::ManualClock;
use js_lease_server::in_process::InProcess;
use js_lease_server::{
    BuildCtx, Built, Config, ElasticBuilt, Floor, KindError, Kinds, OwnerOnlyFiles,
};
use js_types::{ExecMode, Model, ProviderId};
use serde_json::{Value, json};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::elastic::{file_text, offer, queue_job};
use crate::support::{Scratch, Stepping, T0, TOKEN, clock, routed};

/// An acquire held at the gate: the call says it is there on `entered`, then waits until
/// `open` is sent a value (or dropped, when the test ends first).
struct Gate {
    entered: UnboundedSender<()>,
    open: Mutex<Receiver<()>>,
}

/// What a test has the cloud's acquires do: the answers the next acquires are given, in
/// order (once they are used up the cloud answers), and a gate the next acquire waits at.
#[derive(Default)]
struct Script {
    answers: Mutex<VecDeque<Acquired>>,
    gate: Mutex<Option<Gate>>,
}

/// The elastic fake cloud with its acquires as [`Script`] says.
struct Scripted {
    cloud: ElasticFake,
    script: Arc<Script>,
}

impl CapacityProvider for Scripted {
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
        self.cloud.checks()
    }

    fn host_prep(&self, offer: &Offer) -> Vec<BootStep> {
        self.cloud.host_prep(offer)
    }

    fn payload_limit(&self) -> usize {
        self.cloud.payload_limit()
    }

    fn acquire(&self, request: &AcquireRequest) -> Acquired {
        let gate = self.script.gate.lock().unwrap().take();
        if let Some(gate) = gate {
            let _ = gate.entered.send(());
            let _ = gate.open.lock().unwrap().recv();
        }
        let scripted = self.script.answers.lock().unwrap().pop_front();
        scripted.unwrap_or_else(|| self.cloud.acquire(request))
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

    fn events(&self) -> Option<js_elastic::EventStream> {
        self.cloud.events()
    }
}

/// The clouds of these tests, by the path of the key file that names them: a kind is a
/// plain function, so a test leaves its cloud here for the kind to find.
static CLOUDS: Mutex<BTreeMap<String, (ElasticFake, Arc<Script>)>> = Mutex::new(BTreeMap::new());

/// The `logged-cloud` kind of [`file_text`], built here over the cloud a test left in
/// [`CLOUDS`] for the pool's key file, behind [`Scripted`]. Its floor is one `nvidia-a100`,
/// on hosts that run containers.
fn scripted_cloud(table: &toml::Table, _ctx: &BuildCtx<'_>) -> Result<Built, KindError> {
    let path = table.get("api_key_file").and_then(toml::Value::as_str);
    let clouds = CLOUDS.lock().unwrap();
    let (cloud, script) = clouds.get(path.unwrap()).cloned().unwrap();
    Ok(Built::Elastic(ElasticBuilt {
        idle_costs: false,
        provider: Box::new(Scripted { cloud, script }),
        floor: Floor {
            gpus: 1,
            gpu_class: Some(Model::parse("nvidia-a100").unwrap()),
        },
        exec: vec![ExecMode::Container],
    }))
}

/// A started server, its clock and its cloud's script; the scratch directory holds its files.
struct Started {
    server: Arc<InProcess>,
    clock: ManualClock,
    script: Arc<Script>,
    _scratch: Scratch,
}

/// Starts a server with the live pool `burst` of [`file_text`] (and the pool keys `extra`)
/// on a fresh cloud that lists `offers`, with one job queued for an a100 from the start.
/// Nothing has ticked.
fn started(name: &str, offers: Vec<Offer>, extra: &str) -> Started {
    let scratch = Scratch::new(name);
    let cloud = ElasticFake::new(FakeClock::new(1_000_000_000_000), Billing::Lambda, offers);
    let script = Arc::new(Script::default());
    let key = scratch.path("api-key").display().to_string();
    CLOUDS
        .lock()
        .unwrap()
        .insert(key, (cloud, Arc::clone(&script)));
    let text = format!("{}{extra}", file_text(&scratch));
    let path = scratch.write("js.toml", &text, 0o644);
    queue_job(&scratch, "wants-a100");
    let (clock, shared) = clock();
    let kinds = Kinds::standard().with("logged-cloud", scripted_cloud);
    let config = Config::from_config_file(&path, &kinds, &OwnerOnlyFiles, shared.clone()).unwrap();
    let config = config.with_monotonic(Arc::new(Stepping::of(Duration::from_millis(1))));
    let server = Arc::new(InProcess::start(config, shared).unwrap());
    Started {
        server,
        clock,
        script,
        _scratch: scratch,
    }
}

/// The one provider `GET /v1/providers` lists, as the gateway writes it.
async fn the_provider(server: &InProcess) -> Value {
    let router = server.job_router().expect("the job service is on");
    let reply = routed(&router, "GET", "/v1/providers", Some(TOKEN), None).await;
    assert_eq!(reply.status, 200, "{:?}", reply.body);
    let providers = reply.body["providers"].as_array().unwrap();
    assert_eq!(providers.len(), 1, "{providers:?}");
    assert_eq!(providers[0]["name"], "burst");
    providers[0].clone()
}

/// A refusal of the cloud with `code`.
fn refused(code: RefusalCode) -> Acquired {
    Acquired::Refused(Refusal {
        code,
        reason: Reason::Api("scripted".to_owned()),
    })
}

/// The fields this file is about, as the gateway writes them for `provider`; a field the
/// answer leaves out (unset) reads as `null`.
fn held_fields(provider: &Value) -> Value {
    let field = |name: &str| provider.get(name).cloned().unwrap_or(Value::Null);
    json!({
        "passedOver": field("passedOver"),
        "declined": field("declined"),
        "brake": field("brake"),
        "cooldownUntilMs": field("cooldownUntilMs"),
        "cooldownReason": field("cooldownReason"),
        "launching": field("launching"),
    })
}

/// Asserts that after the cloud refuses the queued job's acquire for quota, `ListProviders`
/// tells the offer's instance type passed over in its region with the reason `quota` until
/// the step that took the answer in plus the default `quota_backoff` (1 hour), and nothing
/// else: no decline (the attempt ended, its one offer passed over), no brake and no
/// cool-down (a quota refusal strikes neither). Before the first tick nothing is passed over.
/// Catches a pass-over that is not read from the failover memory, one told with the wrong
/// end, and a quota refusal shown as a brake or cool-down.
#[tokio::test]
async fn a_quota_refusal_is_told_as_an_instance_type_passed_over_until_its_end() {
    let s = started("providers-quota", vec![offer()], "");
    s.script
        .answers
        .lock()
        .unwrap()
        .push_back(refused(RefusalCode::Quota));
    assert_eq!(
        held_fields(&the_provider(&s.server).await)["passedOver"],
        Value::Null
    );

    s.server.tick().await;
    s.server.tick().await;
    let until = (T0 + 3_600_000).to_string();
    let told = json!({
        "passedOver": [{
            "instanceType": "gpu_1x_a100",
            "region": "us-west-1",
            "reason": "quota",
            "untilMs": until,
        }],
        "declined": null,
        "brake": null,
        "cooldownUntilMs": null,
        "cooldownReason": null,
        "launching": null,
    });
    assert_eq!(held_fields(&the_provider(&s.server).await), told);
}

/// Asserts that after the cloud denies the queued job's acquire, on a pool whose brake trips
/// after one failure, `ListProviders` tells the pool's cool-down (its end, the step plus
/// `boot_fail_cooldown`, and the reason `denied`) and the brake engaged since that step with
/// one failure; and that once an operator resets the brake (`POST
/// /v1/pools/burst/brake/reset`) and a tick has applied it, neither is told. Catches a brake
/// or cool-down not read from the scaler's records, one kept after the records dropped it
/// (a copy), and a brake told not engaged after a trip.
#[tokio::test]
async fn a_denied_launch_is_told_as_a_cool_down_and_an_engaged_brake_until_a_reset() {
    let s = started(
        "providers-brake",
        vec![offer()],
        "boot_fail_trip_after = 1\n",
    );
    s.script
        .answers
        .lock()
        .unwrap()
        .push_back(refused(RefusalCode::Denied));
    s.server.tick().await;
    s.server.tick().await;
    let cooldown = s.server.scaler().unwrap().settings().boot_fail_cooldown;
    let until = T0 + u64::try_from(cooldown.as_millis()).unwrap();
    let told = json!({
        "passedOver": null,
        "declined": null,
        "brake": { "engaged": true, "engagedAtMs": T0.to_string(), "failures": 1 },
        "cooldownUntilMs": until.to_string(),
        "cooldownReason": "denied",
        "launching": null,
    });
    assert_eq!(held_fields(&the_provider(&s.server).await), told);

    let client = s.server.client_router();
    let reset = routed(
        &client,
        "POST",
        "/v1/pools/burst/brake/reset",
        Some(TOKEN),
        None,
    )
    .await;
    assert_eq!(reset.status, 202, "{:?}", reset.body);
    s.server.tick().await;
    let fields = held_fields(&the_provider(&s.server).await);
    assert_eq!(fields["brake"], Value::Null, "{fields}");
    assert_eq!(fields["cooldownUntilMs"], Value::Null, "{fields}");
}

/// The offer `id` of the cloud: [`offer`] under another instance type.
fn offer_of(id: &str) -> Offer {
    Offer {
        id: id.to_owned(),
        ..offer()
    }
}

/// Asserts that `ListProviders` tells no catalog before the first fetch; that after the
/// first tick, which fetches the catalog at the clock's time, it tells that time and the 3
/// offers the cloud lists; and that a tick a minute later, before `catalog_refresh` (1 hour)
/// has passed, tells the same fetch time. Catches a fetch time read from the clock rather
/// than the cache, a count of the wrong list, and a catalog told before it was fetched.
#[tokio::test]
async fn the_catalog_is_told_with_its_fetch_time_and_offer_count() {
    let offers = vec![
        offer_of("gpu_1x_a100"),
        offer_of("gpu_2x_a100"),
        offer_of("gpu_4x_a100"),
    ];
    let s = started("providers-catalog", offers, "");
    assert_eq!(the_provider(&s.server).await.get("catalog"), None);

    s.server.tick().await;
    let told = json!({ "fetchedAtMs": T0.to_string(), "offerCount": 3 });
    assert_eq!(the_provider(&s.server).await["catalog"], told);

    s.clock.advance(Duration::from_secs(60));
    s.server.tick().await;
    assert_eq!(the_provider(&s.server).await["catalog"], told);
}

/// Asserts that while the cloud holds the queued job's acquire open, `ListProviders` tells
/// one launch out: the machine's name, the offer and zone, the clock's time at the call, and
/// an age of 7 s after the clock moves 7 s; and that once the cloud answers and the tick
/// ends, no launch is told. Catches a launch out that is not shown while the tick holds the
/// scaler's memory, an age not taken from the server's clock, and an entry left behind after
/// the answer.
#[tokio::test]
async fn a_launch_held_open_by_the_provider_is_told_with_its_age_until_it_is_answered() {
    let s = started("providers-launching", vec![offer()], "");
    let (entered, mut reached): (UnboundedSender<()>, UnboundedReceiver<()>) = unbounded_channel();
    let (open, held): (Sender<()>, Receiver<()>) = std::sync::mpsc::channel();
    *s.script.gate.lock().unwrap() = Some(Gate {
        entered,
        open: Mutex::new(held),
    });
    let server = Arc::clone(&s.server);
    let mut tick = tokio::spawn(async move { server.tick().await });
    tokio::select! {
        _ = reached.recv() => {}
        _ = &mut tick => panic!("the tick ended without calling the cloud's acquire"),
    }

    s.clock.advance(Duration::from_secs(7));
    let launching = the_provider(&s.server).await["launching"].clone();
    let launches = launching.as_array().unwrap();
    assert_eq!(launches.len(), 1, "{launching}");
    let host = launches[0]["host"].as_str().unwrap().to_owned();
    assert!(host.starts_with("js-"), "{host}");
    let told = json!([{
        "host": host,
        "offer": "gpu_1x_a100",
        "zone": "us-west-1",
        "startedAtMs": T0.to_string(),
        "ageMs": "7000",
    }]);
    assert_eq!(launching, told);

    open.send(()).unwrap();
    tick.await.unwrap();
    assert_eq!(the_provider(&s.server).await.get("launching"), None);
}

/// Asserts that after the cloud refuses the queued job's acquire of the a100 in us-west-1 for
/// capacity, and the same step asks the other zone (us-west-2, dearer), `ListProviders`
/// tells the refused offer declined for the job, with the reason `capacity` and the time of
/// the step that took the answer in, while the attempt is in progress; and that nothing is
/// passed over and no brake or cool-down is told (a capacity refusal strikes neither).
/// Catches a decline not read from the failover memory, one told for another provider, and
/// a capacity refusal told as a pass-over.
#[tokio::test]
async fn a_capacity_refusal_is_told_as_an_offer_declined_for_the_waiting_job() {
    let other_zone = Offer {
        zone: Some("us-west-2".to_owned()),
        price_cents_per_hour: Some(200),
        ..offer()
    };
    let s = started("providers-declined", vec![offer(), other_zone], "");
    s.script
        .answers
        .lock()
        .unwrap()
        .push_back(refused(RefusalCode::Capacity));
    s.server.tick().await;
    s.clock.advance(Duration::from_secs(5));
    s.server.tick().await;
    let told = json!({
        "passedOver": null,
        "declined": [{
            "request": "wants-a100",
            "offer": "gpu_1x_a100",
            "zone": "us-west-1",
            "reason": "capacity",
            "declinedAtMs": (T0 + 5000).to_string(),
        }],
        "brake": null,
        "cooldownUntilMs": null,
        "cooldownReason": null,
        "launching": null,
    });
    assert_eq!(held_fields(&the_provider(&s.server).await), told);
}
