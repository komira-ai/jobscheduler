//! A probe (`probe_host`) through `POST /v1/leases` and `DELETE /v1/leases/{id}`, with
//! no socket: the server's routes are called through its router over a registry that
//! places with `js-sched`, on a manual clock, with two registered hosts.

use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::http::{Request as HttpRequest, StatusCode};
use js_agent_wire::{Hello, Phase, ProtocolRange, WireShape};
use js_lease::{Boot, LeaseTable, ManualClock, Settings, Timestamp};
use js_registry::{Credential, PoolConfig, Registry, SequentialSessions};
use js_types::{Admission, Capabilities, ClassId, HostId, ProviderId, ResourceClass, Resources};
use js_types::{Dims, Shape};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::{Shared, router};
use crate::adapt::{BoxedPool, SharedClock};
use crate::agents::Agents;
use crate::config::parse_token;
use crate::sched::place::SchedPlacer;
use js_testkit::dir::TestDir;

/// The client token of the rig's server.
const CLIENT: [u8; 40] = [b'c'; 40];
/// The join token its hosts say hello with.
const JOIN: &str = "t";

fn caps() -> Capabilities {
    Capabilities::from([("arch".to_string(), "x86_64".to_string())])
}

fn share() -> ResourceClass {
    ResourceClass::new(
        ClassId::new("share").unwrap(),
        Admission::Book(Dims::new()),
        0,
    )
}

/// The hello of `host`: 8 cores, 16 GiB, serving.
fn hello(host: &str) -> Hello {
    Hello {
        protocol: ProtocolRange { min: 1, max: 1 },
        host_id: host.into(),
        pool: "builders".into(),
        agent_version: "0.1.0".into(),
        instance: None,
        shape: WireShape {
            cpu_millis: 8000,
            memory_bytes: 16 << 30,
            gpus: 0,
            disk_bytes: 0,
            gpu_class: None,
            max_leases: None,
            accelerator: None,
        },
        capabilities: caps(),
        gpu_devices: Vec::new(),
        labels: Default::default(),
        phase: Phase::Serving,
        held: Vec::new(),
        accepts_host_credential: false,
    }
}

/// A server whose registry books a `share` pool of 4 cores per host, placed with
/// `js-sched`, with hosts `a` and `b` registered.
struct Rig {
    shared: Arc<Shared>,
    registry: Registry,
    /// The rig's own directory (lease and agent state), removed when the rig is dropped.
    /// Last, so it goes after what writes into it.
    dir: TestDir,
}

impl Rig {
    fn new(name: &str) -> Self {
        // A directory of this rig's own: its path carries the process id and a counter
        // beside `name`, so two rigs of one name at once never share or remove it.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/probe_unit");
        let dir = TestDir::under(root, name);
        let clock = ManualClock::new(Timestamp::from_millis(1_000_000));
        let shared_clock = SharedClock::new(clock.clone());
        let resources = Resources::new(4000, 8 << 30, 0, None).unwrap();
        let pool = PoolConfig::new(
            ProviderId::new("builders").unwrap(),
            share(),
            Shape::new(resources, caps()),
            2,
        );
        let registry = Registry::with_sessions(pool, clock, SequentialSessions::new());
        registry.set_placer(Arc::new(SchedPlacer::default()));
        for host in ["a", "b"] {
            registry
                .hello(&hello(host), &Credential::of_token(JOIN))
                .unwrap();
        }
        let provider = registry.provider().with_classes(vec![share()]);
        let settings = Settings::new(Duration::from_secs(300), Duration::from_secs(3600)).unwrap();
        let table = LeaseTable::open(
            BoxedPool(Box::new(provider)),
            shared_clock.clone(),
            settings,
            dir.path().join("leases.toml"),
            Boot::Initialise,
        )
        .unwrap();
        let agents = Agents::over(registry.clone(), &shared_clock, dir.path());
        let token = parse_token(&CLIENT).unwrap();
        let shared = Arc::new(Shared::new(table, token, Some(Arc::new(agents))));
        Self {
            shared,
            registry,
            dir,
        }
    }

    fn cordon(&self, host: &str) {
        let host = HostId::new(host).unwrap();
        self.registry
            .cordon(&host, Duration::from_secs(600))
            .unwrap();
    }

    /// `method path` with `body`, bearing `token` (none for `None`): the status and the
    /// JSON body.
    async fn call(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<&Value>,
    ) -> (StatusCode, Value) {
        let mut request = HttpRequest::builder().method(method).uri(path);
        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        let body = body.map_or_else(Body::empty, |b| Body::from(b.to_string()));
        let reply = router(self.shared.clone(), true)
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap();
        let status = reply.status();
        let bytes = to_bytes(reply.into_body(), 1 << 20).await.unwrap();
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }

    /// `POST /v1/leases` of one core, as the client: with `probe_host` when given.
    async fn acquire(&self, probe: Option<&str>) -> (StatusCode, Value) {
        let mut body = json!({
            "class": "share",
            "resources": { "cpu_millis": 1000, "memory_bytes": 1 << 20 },
            "capabilities": { "arch": "x86_64" },
            "max_wall_seconds": 600,
        });
        if let Some(host) = probe {
            body["probe_host"] = json!(host);
        }
        let client = String::from_utf8(CLIENT.to_vec()).unwrap();
        self.call("POST", "/v1/leases", Some(&client), Some(&body))
            .await
    }
}

/// Asserts the supported path end to end through the routes: a probe for the cordoned
/// host `a` is granted there (201), ordinary leases asked for beside it are granted on
/// `b`, the probe's release frees it (200), and after it the cordon still keeps ordinary
/// work off `a`. It catches a probe the API drops or places elsewhere, and a cordon the
/// probe's release lifts.
#[tokio::test]
async fn a_probe_is_granted_on_its_cordoned_host_and_released() {
    let rig = Rig::new("granted");
    rig.cordon("a");
    let (status, probed) = rig.acquire(Some("a")).await;
    assert_eq!(status, StatusCode::CREATED, "{probed}");
    assert_eq!(probed["host"], "a");
    for _ in 0..2 {
        let (status, ordinary) = rig.acquire(None).await;
        assert_eq!(status, StatusCode::CREATED, "{ordinary}");
        assert_eq!(ordinary["host"], "b");
    }
    let client = String::from_utf8(CLIENT.to_vec()).unwrap();
    let path = format!("/v1/leases/{}", probed["id"].as_str().unwrap());
    let (status, freed) = rig.call("DELETE", &path, Some(&client), None).await;
    assert_eq!(
        (status, freed),
        (StatusCode::OK, json!({ "state": "freed" }))
    );
    let (_, after) = rig.acquire(None).await;
    assert_eq!(after["host"], "b");
    let a = rig.registry.own_host(&HostId::new("a").unwrap()).unwrap();
    assert!(a.cordon_until.is_some(), "the cordon is still in force");
}

/// Asserts that a probe the rule refuses is answered 409 with its code through the
/// routes, a malformed `probe_host` the 400 naming it, and that nothing is booked for
/// either. It catches a code lost between the provider and the reply.
#[tokio::test]
async fn a_refused_probe_is_a_409_with_its_code() {
    let rig = Rig::new("refused");
    for (host, code) in [
        ("nowhere", "probe-host-unknown"),
        ("a", "probe-host-not-cordoned"),
    ] {
        let (status, body) = rig.acquire(Some(host)).await;
        assert_eq!(
            (status, body),
            (StatusCode::CONFLICT, json!({ "error": code }))
        );
    }
    let (status, body) = rig.acquire(Some("Not A Host")).await;
    assert_eq!(
        (status, body),
        (
            StatusCode::BAD_REQUEST,
            json!({ "error": "bad-request", "reason": "probe_host is not a valid host id" })
        )
    );
    let client = String::from_utf8(CLIENT.to_vec()).unwrap();
    let (_, listed) = rig.call("GET", "/v1/leases", Some(&client), None).await;
    assert_eq!(listed, json!({ "leases": [] }));
}

/// Asserts who may ask for a probe: the client token may; no token, another token, the
/// pool's join token and a malformed header are each 401 and book nothing, though the
/// probe's host is cordoned and free. It catches a probe route that trusts an agent's
/// credential or none.
#[tokio::test]
async fn only_the_client_token_may_ask_for_a_probe() {
    let rig = Rig::new("auth");
    rig.cordon("a");
    let body = json!({
        "class": "share",
        "resources": { "cpu_millis": 1000, "memory_bytes": 1 << 20 },
        "capabilities": { "arch": "x86_64" },
        "max_wall_seconds": 600,
        "probe_host": "a",
    });
    let other = "x".repeat(40);
    for token in [None, Some(JOIN), Some(other.as_str())] {
        let (status, _) = rig.call("POST", "/v1/leases", token, Some(&body)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{token:?}");
    }
    let client = String::from_utf8(CLIENT.to_vec()).unwrap();
    let (_, listed) = rig.call("GET", "/v1/leases", Some(&client), None).await;
    assert_eq!(listed, json!({ "leases": [] }));
    let (status, granted) = rig.acquire(Some("a")).await;
    assert_eq!(
        (status, &granted["host"]),
        (StatusCode::CREATED, &json!("a"))
    );
}

/// Asserts that two rigs of one name at once have their own directories and their own
/// leases: a lease taken on one is not listed on the other, and dropping one leaves the
/// other's lease file. It catches a fixture path made of the name alone, which two runs of
/// one test would share and remove under each other.
#[tokio::test]
async fn rigs_of_one_name_have_their_own_directories_and_state() {
    let first = Rig::new("same");
    let second = Rig::new("same");
    assert_ne!(first.dir.path(), second.dir.path());
    let (status, _) = first.acquire(None).await;
    assert_eq!(status, StatusCode::CREATED);
    let client = String::from_utf8(CLIENT.to_vec()).unwrap();
    let (_, listed) = second.call("GET", "/v1/leases", Some(&client), None).await;
    assert_eq!(listed, json!({ "leases": [] }));
    let file = first.dir.path().join("leases.toml");
    drop(second);
    assert!(file.is_file(), "{}", file.display());
    let (_, listed) = first.call("GET", "/v1/leases", Some(&client), None).await;
    assert_eq!(listed["leases"].as_array().map(Vec::len), Some(1));
}
