//! Admission of `POST /v1/leases` (design 5.6, step 2): a request nothing could ever serve
//! is refused `no-capable-host` before anything is recorded; a busy host or a catalog that
//! cannot be read is not a refusal; and a waiting whole-node request whose last capable
//! host goes away keeps its place for the pool's `path_grace`. Time is a manual clock and
//! every tick is stepped by the test; nothing sleeps.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use js_elastic::{
    AcquireRequest, Acquired, BootStep, Capacity, CapacityProvider, CatalogFuture, Held, HostRef,
    Observed, Offer, PriceUnit, ProviderCaps, ProviderError, Released,
};
use js_lease_server::{BuildCtx, Built, Config, ElasticBuilt, Floor, KindError, OwnerOnlyFiles};
use js_types::{ExecMode, HostId, Model, ProviderId, RequestId, accelerator_of};
use serde_json::{Value, json};

use crate::agents_support::{JOIN, hello_body, session, start_agents};
use crate::config_support::{PoolText, file, kinds, quote, quoted, server};
use crate::support::{Harness, Reply, Scratch, lease_body, request};

/// The clouds of these tests, by the path of the key file that names them: a kind is a
/// plain function, so a test leaves its cloud here for the kind to find.
static CLOUDS: Mutex<BTreeMap<String, Arc<Cloud>>> = Mutex::new(BTreeMap::new());

/// A launching provider whose catalog the test sets, offering Linux x86-64 machines with
/// `nvidia-a100` GPUs, that counts its catalog reads and launches nothing.
struct Cloud {
    catalog: Mutex<Result<Vec<Offer>, String>>,
    reads: AtomicUsize,
}

impl Cloud {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            catalog: Mutex::new(Ok(Vec::new())),
            reads: AtomicUsize::new(0),
        })
    }

    fn set(&self, catalog: Result<Vec<Offer>, &str>) {
        *self.catalog.lock().unwrap() = catalog.map_err(str::to_owned);
    }

    fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }
}

/// The provider a pool holds: the test's cloud.
struct Handle(Arc<Cloud>);

impl CapacityProvider for Handle {
    fn id(&self) -> ProviderId {
        ProviderId::known("admission-cloud")
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
        self.0.reads.fetch_add(1, Ordering::SeqCst);
        let answer = self
            .0
            .catalog
            .lock()
            .unwrap()
            .clone()
            .map_err(ProviderError::new);
        Box::pin(std::future::ready(answer))
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
        16 * 1024
    }

    fn acquire(&self, _request: &AcquireRequest) -> Acquired {
        Acquired::Unknown
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

    fn verify_instance(&self, _hello: &js_elastic::Hello) -> Option<HostRef> {
        None
    }
}

/// The `admission-cloud` kind: a pool over the cloud the test left for its key file, with
/// one `nvidia-a100` GPU as its floor.
fn admission_cloud(table: &toml::Table, _ctx: &BuildCtx<'_>) -> Result<Built, KindError> {
    let path = table.get("api_key_file").and_then(toml::Value::as_str);
    let cloud = CLOUDS
        .lock()
        .unwrap()
        .get(path.unwrap_or_default())
        .cloned();
    Ok(Built::Elastic(ElasticBuilt {
        provider: Box::new(Handle(cloud.unwrap())),
        floor: Floor {
            gpus: 1,
            gpu_class: Some(Model::parse("nvidia-a100").unwrap()),
        },
        exec: vec![ExecMode::Container],
    }))
}

/// An offer of one `nvidia-a100` that fits [`one_gpu`], available now or not.
fn a100(available: bool) -> Offer {
    Offer {
        id: "gpu_1x_a100".to_owned(),
        zone: Some("zone-a".to_owned()),
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
        available,
        purchase: None,
        quota: None,
        block: None,
    }
}

/// A server whose one pool, `burst`, of `class`, launches (in plan) on a fresh cloud and
/// takes hosts that join with [`JOIN`], with `more` keys set; the cloud with it.
async fn elastic(name: &str, class: &str, more: &[(&str, &str)]) -> (Harness, Arc<Cloud>) {
    over(name, class, more, None).await
}

/// [`elastic`], with the cloud declared as the machine provider `cloud` of the platform
/// `ours` when `allowed` is given, and the pool allowing it the instance types `allowed`
/// (a TOML array).
async fn over(
    name: &str,
    class: &str,
    more: &[(&str, &str)],
    allowed: Option<&str>,
) -> (Harness, Arc<Cloud>) {
    let scratch = Scratch::new(name);
    let cloud = Cloud::new();
    let key = scratch.write("cloud-key", "unused", 0o600);
    CLOUDS
        .lock()
        .unwrap()
        .insert(key.display().to_string(), cloud.clone());
    let joins = scratch.write("joins", &format!("{JOIN}\n"), 0o600);
    let provider = format!(
        "{{ kind = \"admission-cloud\", api_key_file = {} }}",
        quoted(&key)
    );
    let mut pool = PoolText::elastic(&scratch)
        .set("name", &quote("burst"))
        .set("class", &quote(class))
        .set("exec", &quote("none"))
        .set("provider", &provider)
        .set("join_token_file", &quoted(&joins));
    for (key, value) in more {
        pool = pool.set(key, value);
    }
    let tables = match allowed {
        Some(types) => {
            pool = pool.without("provider").set("platforms", "[\"ours\"]").set(
                "provider_limits",
                &format!("{{ cloud = {{ allowed_instance_types = {types} }} }}"),
            );
            format!(
                "[[machine_provider]]\nname = \"rack\"\nkind = \"static\"\n\n\
                 [[machine_provider]]\nname = \"cloud\"\nkind = \"admission-cloud\"\napi_key_file = {}\n\n\
                 [[platform]]\nname = \"ours\"\nkind = \"default\"\nmachine_providers = [\"rack\", \"cloud\"]\n",
                quoted(&key)
            )
        }
        None => String::new(),
    };
    let head = server(
        &scratch,
        "listen = \"127.0.0.1:0\"\nagent_listen = \"127.0.0.2:0\"",
    );
    let text = format!("{}\n{tables}", file(&head, &[&pool]));
    let path = scratch.write("js.toml", &text, 0o644);
    let kinds = kinds().with("admission-cloud", admission_cloud);
    let clock = crate::config_support::clock();
    let config = Config::from_config_file(&path, &kinds, &OwnerOnlyFiles, clock).unwrap();
    (Harness::over(scratch, config).await, cloud)
}

/// A request for one `nvidia-a100` of `class`, naming no capability.
fn one_gpu(class: &str) -> Value {
    json!({
        "class": class,
        "resources": { "cpu_millis": 1000, "memory_bytes": 1024, "gpus": 1, "gpu_class": "nvidia-a100" },
        "max_wall_seconds": 600,
    })
}

/// A whole-node request for one `nvidia-a100` under `request_id`.
fn whole(request_id: &str) -> Value {
    let mut body = one_gpu("whole-node");
    body["request_id"] = json!(request_id);
    body
}

/// [`lease_body`] asking for `gpus` GPUs.
fn gpus(count: u64) -> Value {
    let mut body = lease_body();
    body["resources"]["gpus"] = json!(count);
    body
}

async fn post(h: &Harness, body: &Value) -> Reply {
    h.call("POST", "/v1/leases", Some(body)).await
}

/// Asserts that `reply` is a 409 with the error `code`.
fn refused(reply: &Reply, code: &str) {
    assert_eq!(reply.status, 409, "{}", reply.body);
    assert_eq!(reply.json(), json!({ "error": code }));
}

/// The bytes of the lease file.
fn lease_file(h: &Harness) -> Vec<u8> {
    std::fs::read(h.scratch.state()).unwrap()
}

/// A hello of `host` on the agent port of an elastic server, which must be admitted, and
/// its first heartbeat.
async fn join_agent(h: &Harness, host: &str) {
    let agent = h.running.agent_addr().unwrap();
    let hello = hello_body(host).to_string().into_bytes();
    let reply = request(agent, "POST", "/v1/agent/hello", Some(JOIN), Some(&hello)).await;
    let joined = session(&reply, host);
    let beat = joined.beat().to_string().into_bytes();
    let reply = request(
        agent,
        "POST",
        "/v1/agent/heartbeat",
        Some(JOIN),
        Some(&beat),
    )
    .await;
    assert_eq!(reply.status, 200, "{}", reply.body);
}

/// Whether `GET /v1/leases` lists the lease `id`.
async fn listed(h: &Harness, id: &str) -> bool {
    let listed = h.call("GET", "/v1/leases", None).await.json();
    let leases = listed["leases"].as_array().unwrap().clone();
    leases.iter().any(|lease| lease["id"] == id)
}

/// The lease id of a 202 answer.
fn pending(reply: &Reply) -> String {
    assert_eq!(reply.status, 202, "{}", reply.body);
    reply.json()["id"].as_str().unwrap().to_owned()
}

/// The lease host `h1` holds, as `GET /v1/hosts` lists it.
async fn lease_on_h1(h: &Harness) -> Value {
    let listed = h.call("GET", "/v1/hosts", None).await.json();
    let hosts = listed["hosts"].as_array().unwrap().clone();
    let host = hosts.into_iter().find(|host| host["id"] == "h1").unwrap();
    host["lease"].clone()
}

/// Asserts, through the route, that a pool whose only machine provider is `static` (hosts
/// that register themselves) refuses `no-capable-host` with no host registered, and with a
/// registered host too small for the request, each time leaving the lease file as it was;
/// that a request breaking the start contract keeps its own refusal there (422
/// `unknown-class`, not `no-capable-host`); and that a capable host that is busy admits the
/// request, which the start then answers `no-capacity` as before. Catches a static provider counted as one that may offer, and a
/// busy host read as absent.
#[tokio::test]
async fn a_static_only_pool_without_a_capable_host_refuses() {
    let h = start_agents("admission_static_only", false).await;
    let before = lease_file(&h);
    refused(&post(&h, &lease_body()).await, "no-capable-host");
    assert_eq!(lease_file(&h), before);
    let mut unknown = lease_body();
    unknown["class"] = json!("no-such-class");
    let contract = post(&h, &unknown).await;
    assert_eq!(contract.status, 422, "{}", contract.body);
    assert_eq!(contract.json(), json!({ "error": "unknown-class" }));
    let session = crate::agents_support::join(&h, "h1").await;
    let beat = crate::agents_support::beat(&h, &session).await;
    assert_eq!(beat.status, 200, "{}", beat.body);
    let before = lease_file(&h);
    refused(&post(&h, &gpus(2)).await, "no-capable-host");
    assert_eq!(lease_file(&h), before);
    assert_eq!(post(&h, &lease_body()).await.status, 201);
    refused(&post(&h, &lease_body()).await, "no-capacity");
}

/// Asserts that a refusal at admission records nothing: the lease file's bytes are the
/// same before and after, and the next lease granted takes the id it would have taken had
/// the refused request never come (`1-1`, the first of the term).
#[tokio::test]
async fn a_refusal_records_nothing() {
    let h = Harness::new("admission_records_nothing").await;
    let before = lease_file(&h);
    refused(&post(&h, &gpus(16)).await, "no-capable-host");
    assert_eq!(lease_file(&h), before);
    let granted = post(&h, &lease_body()).await;
    assert_eq!(granted.status, 201, "{}", granted.body);
    assert_eq!(granted.json()["id"], "1-1");
}

/// Asserts the answers of a request a host of the static pool can serve, with no
/// `request_id`, are those it always had: the two hosts granted in order, then
/// `no-capacity` for the third, whose host is only busy.
#[tokio::test]
async fn an_ordinary_request_with_no_request_id_still_gets_the_same_answers_as_before_when_a_host_can_serve_it()
 {
    let h = Harness::new("admission_same_answers").await;
    for (id, host) in [("1-1", "gpu-01"), ("1-2", "gpu-02")] {
        let granted = post(&h, &lease_body()).await;
        assert_eq!(granted.status, 201, "{}", granted.body);
        assert_eq!(granted.json()["id"], id);
        assert_eq!(granted.json()["host"], host);
    }
    refused(&post(&h, &lease_body()).await, "no-capacity");
}

/// Asserts, through the route, how admission reads the cached catalog and never the
/// provider's: before any tick the catalog is unread and a request only a catalog could
/// serve is admitted (the start answers `no-capacity`); after a tick whose fetch failed it
/// is still unread, and that fetch is retried at the next tick; a tick that fetches an empty
/// catalog makes the request `no-capable-host`, leaving the lease file as it was; that
/// catalog is kept until the clock has moved past `catalog_refresh`, after which a tick
/// fetches the one fitting offer, unavailable now, and the request is `no-capacity` again;
/// and the cloud's reads move at ticks only, never across the posts. Catches an unreadable
/// catalog read as an empty one, admission asking whether an offer is available now, and
/// admission calling the provider.
#[tokio::test]
async fn an_unreadable_catalog_counts_as_may_offer() {
    let (h, cloud) = elastic("admission_unreadable", "gpu-host", &[]).await;
    let request = one_gpu("gpu-host");
    cloud.set(Err("the cloud's API did not answer"));
    let reads = cloud.reads();
    refused(&post(&h, &request).await, "no-capacity");
    refused(&post(&h, &request).await, "no-capacity");
    assert_eq!(cloud.reads(), reads);
    h.ticks.step().await;
    assert_eq!(cloud.reads(), reads + 1);
    refused(&post(&h, &request).await, "no-capacity");
    assert_eq!(cloud.reads(), reads + 1);
    cloud.set(Ok(Vec::new()));
    h.ticks.step().await;
    assert_eq!(cloud.reads(), reads + 2);
    let before = lease_file(&h);
    refused(&post(&h, &request).await, "no-capable-host");
    assert_eq!(cloud.reads(), reads + 2);
    assert_eq!(lease_file(&h), before);
    cloud.set(Ok(vec![a100(false)]));
    h.ticks.step().await;
    assert_eq!(cloud.reads(), reads + 2);
    refused(&post(&h, &request).await, "no-capable-host");
    h.clock.advance(Duration::from_secs(2 * 3600));
    h.ticks.step().await;
    assert_eq!(cloud.reads(), reads + 3);
    refused(&post(&h, &request).await, "no-capacity");
    assert_eq!(cloud.reads(), reads + 3);
}

/// Asserts that a pool's `provider_limits` bound what admits: the one fitting offer, of an
/// instance type the pool does not allow, refuses `no-capable-host`, and with a pool that
/// allows that type the same request is admitted (the start answers `no-capacity`, as there
/// is no host). Catches limits that admission does not read.
#[tokio::test]
async fn an_offer_outside_the_pool_limits_does_not_count() {
    let other = Some("[\"gpu_8x_a100\"]");
    let (h, cloud) = over("admission_limits_out", "gpu-host", &[], other).await;
    cloud.set(Ok(vec![a100(true)]));
    h.ticks.step().await;
    refused(&post(&h, &one_gpu("gpu-host")).await, "no-capable-host");
    let allowed = Some("[\"gpu_1x_a100\"]");
    let (h, cloud) = over("admission_limits_in", "gpu-host", &[], allowed).await;
    cloud.set(Ok(vec![a100(true)]));
    h.ticks.step().await;
    refused(&post(&h, &one_gpu("gpu-host")).await, "no-capacity");
}

/// Starts a whole-node pool with a `path_grace` of one second and one host, `h1`; queues a
/// whole-node request `r1` behind `r0`, which holds `h1`; ends `r0`; and removes `h1` from
/// the host table, as when its machine is given back. The next tick finds no capable host
/// for `r1`, which is still waiting, first in the queue.
async fn host_gone(name: &str) -> (Harness, String) {
    let grace = [("path_grace", "\"1s\"")];
    let (h, _cloud) = elastic(name, "whole-node", &grace).await;
    join_agent(&h, "h1").await;
    let held = pending(&post(&h, &whole("r0")).await);
    let waiting = pending(&post(&h, &whole("r1")).await);
    let registry = h.running.registry().unwrap();
    let r1 = RequestId::new("r1").unwrap();
    assert_eq!(registry.queue_position(&r1), Some(0));
    let freed = h.call("DELETE", &format!("/v1/leases/{held}"), None).await;
    assert_eq!(freed.status, 200, "{}", freed.body);
    let h1 = HostId::new("h1").unwrap();
    registry.mark_ending(&h1);
    registry.remove(&h1).unwrap();
    h.ticks.step().await;
    assert!(listed(&h, &waiting).await);
    assert_eq!(registry.queue_position(&r1), Some(0));
    (h, waiting)
}

/// Asserts that a waiting request whose only capable host went away keeps its place while
/// the pool's `path_grace` runs, and that when the host registers again with the same
/// facts just inside the grace the request is placed on it. Catches a refusal the moment
/// the last host goes, which would turn a routine restart into failed work.
#[tokio::test]
async fn path_grace_keeps_a_job_when_the_last_host_restarts() {
    let (h, waiting) = host_gone("admission_grace_keeps").await;
    h.clock.advance(Duration::from_millis(900));
    h.ticks.step().await;
    let r1 = RequestId::new("r1").unwrap();
    let registry = h.running.registry().unwrap();
    assert_eq!(registry.queue_position(&r1), Some(0));
    join_agent(&h, "h1").await;
    h.ticks.step().await;
    assert!(listed(&h, &waiting).await);
    assert_eq!(registry.queue_position(&r1), None);
    assert_eq!(lease_on_h1(&h).await, json!(waiting));
}

/// Asserts that a waiting request whose only capable host went away and did not come back
/// ends once the pool's `path_grace` has passed: the tick drops its lease, and the same
/// request sent again is refused `no-capable-host` at admission. Catches a request kept
/// waiting for ever for a host nobody will bring back.
#[tokio::test]
async fn path_grace_ends_a_job_when_no_path_returns() {
    let (h, waiting) = host_gone("admission_grace_ends").await;
    h.clock.advance(Duration::from_millis(900));
    h.ticks.step().await;
    assert!(listed(&h, &waiting).await);
    h.clock.advance(Duration::from_millis(200));
    h.ticks.step().await;
    assert!(!listed(&h, &waiting).await);
    let registry = h.running.registry().unwrap();
    assert_eq!(
        registry.queue_position(&RequestId::new("r1").unwrap()),
        None
    );
    refused(&post(&h, &whole("r1")).await, "no-capable-host");
}
