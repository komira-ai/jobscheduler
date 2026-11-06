//! The job service built from the server's state, and the router of its port, with no
//! socket: a server state over a lease table and an agent side in a directory of the test's
//! own, and requests sent to the router as a `tower::Service`.

use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use js_jobs::{JobId, NewJob};
use js_lease::{Boot, Clock, LeaseTable, ManualClock, Settings, Timestamp};
use js_registry::{PoolConfig, Registry, SequentialSessions};
use js_testkit::dir::TestDir;
use js_types::{ProviderId, ResourceClass, Resources, Shape};
use tower::ServiceExt;

use super::{from_server, port};
use crate::adapt::{BoxedPool, SharedClock};
use crate::agent_api::job_rig::{spec, tables};
use crate::agents::Agents;
use crate::api::Shared;
use crate::config::{Token, parse_token};

/// The time the server's clock reads, in milliseconds since the Unix epoch.
const NOW_MS: u64 = 1_700_000_000_000;

/// The client token of the server under test.
const TOKEN: [u8; 40] = [b'c'; 40];

fn token() -> Token {
    parse_token(&TOKEN).unwrap()
}

/// A server state in `dir` whose agent side holds `jobs` (when given), or that has no agent
/// side at all when `agents` is false.
fn state(dir: &TestDir, agents: bool, jobs: Option<Arc<crate::job_store::ServerJobs>>) -> Shared {
    let clock = SharedClock::new(ManualClock::new(Timestamp::from_millis(NOW_MS)));
    let resources = Resources::new(4000, 8 << 30, 0, None).unwrap();
    let pool = PoolConfig::new(
        ProviderId::new("builders").unwrap(),
        ResourceClass::gpu_host(),
        Shape::new(resources, Default::default()),
        2,
    );
    let registry = Registry::with_sessions(pool, clock.clone(), SequentialSessions::new());
    let settings = Settings::new(Duration::from_secs(300), Duration::from_secs(3600)).unwrap();
    let table = LeaseTable::open(
        BoxedPool(Box::new(registry.provider())),
        clock.clone(),
        settings,
        dir.path().join("leases.toml"),
        Boot::Initialise,
    )
    .unwrap();
    let side = Agents::over(registry, &clock, dir.path());
    let side = match jobs {
        Some(jobs) => side.with_jobs(jobs),
        None => side,
    };
    let side = agents.then(|| Arc::new(side));
    Shared::new(table, token(), side)
}

/// The ceiling the service is built with: one hour, so that it differs from the default.
const WALL: std::time::Duration = std::time::Duration::from_secs(3600);

fn scratch(name: &str) -> TestDir {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/job_wiring");
    TestDir::under(root, name)
}

/// Asserts that the job service built from the server's state holds the very job store
/// handle the state holds (the same allocation, not a second store over the same data),
/// that a job recorded through the state's handle is read back through the service's, and
/// that the service reads the server's clock. It catches a service that opens or is given
/// a job store of its own, which the heartbeat path would not see.
#[test]
fn the_job_store_is_one_handle_shared_by_the_service() {
    let dir = scratch("one_handle");
    let (_, jobs) = tables();
    let state = state(&dir, true, Some(Arc::new(jobs)));
    let api = from_server(&state, WALL).expect("the server holds a job store");
    let held = state.agents().unwrap().jobs.as_ref().unwrap();
    assert!(Arc::ptr_eq(&api.jobs, held));

    let id = JobId::parse("job-through-the-state").unwrap();
    let new = NewJob {
        id: Some(id.clone()),
        spec: spec(),
        priority: 0,
        client: None,
    };
    assert!(held.insert_queued(new).unwrap().created);
    let seen = api
        .jobs
        .get_job(&id)
        .unwrap()
        .expect("the service sees the job");
    assert_eq!(seen.id, id);
    assert_eq!(api.clock.now(), Timestamp::from_millis(NOW_MS));
    assert!(
        api.read.silence.is_some(),
        "GetJob would show no host silence"
    );
    assert_ne!(
        api.submit,
        super::submit::Ctx::default(),
        "SubmitJob is not served"
    );
    assert_eq!(api.submit.ceiling(), Some(WALL));
}

/// Asserts that no job service is built from a server that holds no job store: one whose
/// agent side has none (the job service is off), and one with no agent side at all. It
/// catches a service built over a store of its own when the server has none.
#[test]
fn without_a_job_store_the_server_builds_no_job_service() {
    let dir = scratch("no_store");
    assert!(from_server(&state(&dir, true, None), WALL).is_none());
    let other = scratch("no_agents");
    assert!(from_server(&state(&other, false, None), WALL).is_none());
}

/// The status, the `grpc-status` header (absent for a plain HTTP answer) and the body of
/// the port's answer to `GET /v1/jobs/job-1` sent with `content_type` (none when `None`)
/// and, when `authorized`, the server's token.
async fn answer(
    content_type: Option<&str>,
    authorized: bool,
) -> (StatusCode, Option<String>, String) {
    let dir = scratch("port");
    let (_, jobs) = tables();
    let state = state(&dir, true, Some(Arc::new(jobs)));
    let clients = crate::config::Clients::only(&token(), "--token-file");
    let clients = crate::config::LiveClients::new(clients);
    let router = port(from_server(&state, WALL).unwrap(), clients);
    let mut request = Request::get("/v1/jobs/job-1");
    if let Some(content_type) = content_type {
        request = request.header(header::CONTENT_TYPE, content_type);
    }
    if authorized {
        let bearer = format!("Bearer {}", String::from_utf8(TOKEN.to_vec()).unwrap());
        request = request.header(header::AUTHORIZATION, bearer);
    }
    let response = router
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let grpc = response
        .headers()
        .get("grpc-status")
        .map(|value| value.to_str().unwrap().to_owned());
    let body = to_bytes(response.into_body(), 1 << 16).await.unwrap();
    (status, grpc, String::from_utf8(body.to_vec()).unwrap())
}

/// Asserts that the port hands a request to the transport its content type names: with
/// `application/grpc` (alone or with a suffix) to the gRPC service, which answers
/// `UNIMPLEMENTED` (12: no method has the gateway's path) to a caller with the token and
/// `UNAUTHENTICATED` (16) to one without, as a gRPC status in an HTTP 200; with any other
/// content type, or none, to the gateway, which answers 404 `unknown-job` (no job is stored)
/// with the token and 401 with an empty body without it.
/// It catches a port that sends every request one way, or that reads the content type
/// another way than the bearer layer does.
#[tokio::test]
async fn the_port_routes_by_content_type() {
    for grpc in ["application/grpc", "application/grpc+proto"] {
        let (status, code, _) = answer(Some(grpc), true).await;
        assert_eq!(
            (status, code.as_deref()),
            (StatusCode::OK, Some("12")),
            "{grpc}"
        );
        let (status, code, _) = answer(Some(grpc), false).await;
        assert_eq!(
            (status, code.as_deref()),
            (StatusCode::OK, Some("16")),
            "{grpc}"
        );
    }
    for other in [Some("application/json"), Some("text/plain"), None] {
        let (status, code, body) = answer(other, true).await;
        assert_eq!(
            (status, code, body.as_str()),
            (StatusCode::NOT_FOUND, None, r#"{"error":"unknown-job"}"#),
            "{other:?}"
        );
        let (status, code, body) = answer(other, false).await;
        assert_eq!(
            (status, code, body.as_str()),
            (StatusCode::UNAUTHORIZED, None, ""),
            "{other:?}"
        );
    }
}
