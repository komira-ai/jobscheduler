//! Admission of `POST /v1/jobs` (design 5.6, step 2): a job nothing could ever serve is
//! refused `no-capable-host` before anything is recorded; a busy host or a catalog that
//! cannot be read is not a refusal. Time is a manual clock and every tick is stepped by the
//! test; nothing sleeps.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use js_elastic::{
    AcquireRequest, Acquired, BootStep, Capacity, CapacityProvider, CatalogFuture, Held, HostRef,
    Observed, Offer, PriceUnit, ProviderCaps, ProviderError, Released,
};
use js_lease_server::{BuildCtx, Built, Config, ElasticBuilt, Floor, KindError, OwnerOnlyFiles};
use js_types::{ExecMode, Model, ProviderId, accelerator_of};
use serde_json::{Value, json};

use crate::agents_support::{JOIN, agent_args, join};
use crate::config_support::{PoolText, file, kinds, quote, quoted, server};
use crate::jobs_support::{JobBody, job_of, submit, submit_body};
use crate::support::{Harness, Reply, Scratch, TOKEN, request};

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
        idle_costs: false,
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
/// takes hosts that join with [`JOIN`], with `more` keys set, and whose job service is on a
/// loopback port; the cloud with it.
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
    let listen = "listen = \"127.0.0.1:0\"\nagent_listen = \"127.0.0.2:0\"";
    let head = server(&scratch, &format!("{listen}\njob_listen = \"127.0.0.1:0\""));
    let text = format!("{}\n{tables}", file(&head, &[&pool]));
    let path = scratch.write("js.toml", &text, 0o644);
    let kinds = kinds().with("admission-cloud", admission_cloud);
    let clock = crate::config_support::clock();
    let config = Config::from_config_file(&path, &kinds, &OwnerOnlyFiles, clock).unwrap();
    (Harness::over(scratch, config).await, cloud)
}

/// A job `id` for one `nvidia-a100`, naming no constraint.
fn one_gpu(id: &str) -> Value {
    JobBody::new(id)
        .needs(1000, 1024)
        .gpus(1, "nvidia-a100")
        .json()
}

/// Asserts that `reply` is a 422 `no-capable-host`.
fn refused(reply: &Reply) {
    assert_eq!(reply.status, 422, "{}", reply.body);
    assert_eq!(reply.json(), json!({ "error": "no-capable-host" }));
}

/// Asserts that `reply` queued a job.
fn admitted(reply: &Reply) {
    assert_eq!(reply.status, 201, "{}", reply.body);
    assert_eq!(reply.json()["job"]["state"], "JOB_STATE_QUEUED");
}

/// The jobs `GET /v1/jobs` lists on the job port of `h`, by id.
async fn listed(h: &Harness) -> Vec<String> {
    let addr = h.running.job_addr().unwrap();
    let reply = request(addr, "GET", "/v1/jobs", Some(TOKEN), None).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    let jobs = reply.json()["jobs"].as_array().cloned().unwrap_or_default();
    jobs.iter()
        .map(|job| job["id"].as_str().unwrap().to_owned())
        .collect()
}

/// The flags of a server with agent registration on and the job service on
/// a loopback port.
fn jobs_without_static(scratch: &Scratch) -> Vec<std::ffi::OsString> {
    let mut args = agent_args(scratch);
    args.extend(["--job-listen", "127.0.0.1:0"].map(Into::into));
    args
}

/// Asserts, through the job service, that a pool whose only machine provider is `static`
/// (hosts that register themselves) refuses `no-capable-host` with no host registered, and
/// with a registered host too small for the job, each time recording nothing; and that a
/// capable host admits the job. Catches a static provider counted as one that may offer.
#[tokio::test]
async fn a_static_only_pool_without_a_capable_host_refuses() {
    let scratch = Scratch::new("admission_static_only");
    let args = jobs_without_static(&scratch);
    let h = Harness::with(scratch, args).await;
    refused(&submit_body(&h, &one_gpu("no-host")).await);
    assert_eq!(job_of(&h, "no-host").await.status, 404);
    join(&h, "h1").await;
    let too_big = JobBody::new("two-gpus").gpus(2, "nvidia-a100").json();
    refused(&submit_body(&h, &too_big).await);
    assert_eq!(job_of(&h, "two-gpus").await.status, 404);
    assert_eq!(listed(&h).await, Vec::<String>::new());
    admitted(&submit_body(&h, &one_gpu("fits")).await);
    admitted(&submit_body(&h, &one_gpu("fits-too")).await);
    assert_eq!(listed(&h).await, ["fits", "fits-too"]);
}

/// Asserts that a refusal at admission records nothing: after a job nothing could ever
/// serve is refused, the job reads 404 and the list of jobs is empty, and the next job that
/// a host can serve is queued.
#[tokio::test]
async fn a_refusal_records_nothing() {
    let scratch = Scratch::new("admission_records_nothing");
    let args = jobs_without_static(&scratch);
    let h = Harness::with(scratch, args).await;
    join(&h, "h1").await;
    let too_big = JobBody::new("sixteen").gpus(16, "nvidia-a100").json();
    refused(&submit_body(&h, &too_big).await);
    assert_eq!(job_of(&h, "sixteen").await.status, 404);
    assert_eq!(listed(&h).await, Vec::<String>::new());
    admitted(&submit_body(&h, &one_gpu("after")).await);
    assert_eq!(listed(&h).await, ["after"]);
}

/// Asserts, through the job service's port of a server whose pool launches on the test's
/// cloud and has no host, that a submitted job is admitted over the pool's cached catalog:
/// before any fetch, and after a fetch that failed, the catalog is unread and may
/// offer, so the job is queued (201); once a tick has fetched an empty catalog the job is
/// refused `no-capable-host` (422) and nothing is recorded, so the job is unknown; once a tick
/// past `catalog_refresh` has fetched an offer that fits, unavailable now, the same job is
/// admitted. The cloud is read at ticks only, never by a submit. It catches an unreadable
/// catalog read as empty, a job service that admits over the hosts alone, and a submit that
/// calls the provider.
#[tokio::test]
async fn a_submitted_job_is_admitted_over_the_catalog() {
    let (h, cloud) = elastic("admission_jobs", "gpu-host", &[]).await;
    cloud.set(Err("the cloud's API did not answer"));
    let reads = cloud.reads();
    let queued = submit(&h, "job-unfetched").await;
    assert_eq!(queued.status, 201, "{}", queued.body);
    assert_eq!(queued.json()["job"]["state"], "JOB_STATE_QUEUED");
    h.ticks.step().await;
    assert_eq!(cloud.reads(), reads + 1);
    let queued = submit(&h, "job-unread").await;
    assert_eq!(queued.status, 201, "{}", queued.body);
    cloud.set(Ok(Vec::new()));
    h.ticks.step().await;
    assert_eq!(cloud.reads(), reads + 2);
    let refused = submit(&h, "job-empty").await;
    assert_eq!(
        (refused.status, refused.json()),
        (422, json!({ "error": "no-capable-host" }))
    );
    let job = h.running.job_addr().unwrap();
    let unknown = request(job, "GET", "/v1/jobs/job-empty", Some(TOKEN), None).await;
    assert_eq!(unknown.status, 404, "{}", unknown.body);
    cloud.set(Ok(vec![a100(false)]));
    h.clock.advance(Duration::from_secs(2 * 3600));
    h.ticks.step().await;
    assert_eq!(cloud.reads(), reads + 3);
    let offered = submit(&h, "job-empty").await;
    assert_eq!(offered.status, 201, "{}", offered.body);
    assert_eq!(cloud.reads(), reads + 3);
}

/// Asserts that a pool's `provider_limits` bound what admits: the one fitting offer, of an
/// instance type the pool does not allow, refuses `no-capable-host`, and with a pool that
/// allows that type the same job is admitted. Catches limits that admission does not read.
#[tokio::test]
async fn an_offer_outside_the_pool_limits_does_not_count() {
    let other = Some("[\"gpu_8x_a100\"]");
    let (h, cloud) = over("admission_limits_out", "gpu-host", &[], other).await;
    cloud.set(Ok(vec![a100(true)]));
    h.ticks.step().await;
    refused(&submit_body(&h, &one_gpu("limits-out")).await);
    assert_eq!(job_of(&h, "limits-out").await.status, 404);
    let allowed = Some("[\"gpu_1x_a100\"]");
    let (h, cloud) = over("admission_limits_in", "gpu-host", &[], allowed).await;
    cloud.set(Ok(vec![a100(true)]));
    h.ticks.step().await;
    admitted(&submit_body(&h, &one_gpu("limits-in")).await);
}
