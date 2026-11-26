//! A host's liveness as two servers over one store judge it from the host's row `node/<host>`
//! (design `multi-instance.md`, G5): the job service's `host_silent_since_ms` on either server,
//! and the `host-live` refusal of a hello whose row another server's hello wrote first.

use js_proto::v1::job_service_server::JobService;
use js_proto::v1::{GetJobRequest, ListJobsRequest};
use tonic::Request;

use super::*;
use crate::agent_api::job_rig::{BOOKED, placed, spec};
use crate::job_api::JobApi;
use crate::job_api::read::Ctx;
use crate::job_api::read::silence::Silence;
use crate::job_api::read::view::millis;
use crate::job_api::roles::as_operator;

/// The pool's grace: the default, as [`World::server`] makes its pool.
const GRACE: Duration = Duration::from_millis(js_agent_wire::DEFAULT_GRACE_MS);

/// The job service of `server`, reading host silence as the server is wired to.
fn api_of(world: &World, server: &Server) -> JobApi {
    let jobs = server.agents.jobs.clone().expect("the job tables");
    let shared = SharedClock::new(OneClock(world.clock.clone()));
    let read = Ctx::default().with_silence(Silence::of(server.agents.registry.clone()));
    JobApi::new(jobs, shared).with_read(read)
}

/// The silence `GetJob` and `ListJobs` of `api` show on the job `id`, which must be listed.
async fn shown(api: &JobApi, id: &JobId) -> (Option<u64>, Option<u64>) {
    let asked = as_operator(Request::new(GetJobRequest {
        job_id: id.to_string(),
    }));
    let job = api.get_job(asked).await.unwrap().into_inner().job.unwrap();
    let asked = as_operator(Request::new(ListJobsRequest::default()));
    let listed = api.list_jobs(asked).await.unwrap().into_inner().jobs;
    let listed = listed.into_iter().find(|one| one.id == id.to_string());
    (
        job.host_silent_since_ms,
        listed.unwrap().host_silent_since_ms,
    )
}

/// When the grace of the row of `host`, as it stands, runs out: in milliseconds.
fn missed(world: &World, host: &str) -> u64 {
    millis(world.row(host).record.heard_at.saturating_add(GRACE))
}

/// Asserts that a job's `host_silent_since_ms` is the same on both servers, whichever took the
/// host's hello and beats: none while the row was heard within the grace, the instant the grace
/// ran out once it was not (on the server whose registry never held the host too), and none
/// again on both once a beat lands on the other server (the first server's registry never heard
/// it). Catches a silence read from the registry of the server asked, which shows none for a
/// host another server holds and a silence for one whose beats moved to another server.
#[tokio::test]
async fn a_jobs_host_silence_is_the_rows_on_every_server() {
    let world = World::new();
    let first = world.server("rows_live_silence_a", Kind::JOBS);
    let second = world.server("rows_live_silence_b", Kind::JOBS);
    let session = first.join("host-a").await;
    first.beat(&session, Some(1), Vec::new()).await.unwrap();
    let (job, _) = placed(&world.jobs, "job-silent", "host-a", spec(), BOOKED);
    let apis = [api_of(&world, &first), api_of(&world, &second)];
    for api in &apis {
        assert_eq!(shown(api, &job.id).await, (None, None), "just heard");
    }

    let at = missed(&world, "host-a");
    world.advance(GRACE + Duration::from_millis(1));
    for api in &apis {
        assert_eq!(
            shown(api, &job.id).await,
            (Some(at), Some(at)),
            "past the grace"
        );
    }

    second.beat(&session, Some(2), Vec::new()).await.unwrap();
    for api in &apis {
        assert_eq!(
            shown(api, &job.id).await,
            (None, None),
            "heard on the other server"
        );
    }
    let at = missed(&world, "host-a");
    world.advance(GRACE + Duration::from_millis(1));
    for api in &apis {
        assert_eq!(
            shown(api, &job.id).await,
            (Some(at), Some(at)),
            "silent again"
        );
    }
}

/// The body of the hello of `host` by the agent process `process`.
fn hello_by(host: &str, process: &str) -> Bytes {
    let mut hello: Value = serde_json::from_slice(&hello_body(host)).unwrap();
    hello["agent_process"] = json!(process);
    Bytes::from(hello.to_string())
}

/// Asserts that a hello the registry judged on a row with no live session is refused 409
/// `host-live` when, before its row is written, another server writes the hello of another
/// agent process of the host: the row stays the other's, and the registry gives the session up,
/// so the next hello of the row's own process through the same server takes the host over at
/// once. Catches a hello whose row write does not judge the row again (two agent processes
/// then hold one host, each through its own server), and one refused `state-write-failed`,
/// which the agent retries as if the store were down.
#[tokio::test]
async fn a_hello_whose_row_another_process_took_first_is_host_live() {
    let world = World::new();
    let server = world.server("rows_live_hello", Kind::JOBS);
    let theirs = NodeHello {
        session: None,
        pool: "burst".to_owned(),
        claim: None,
        shape: None,
        labels: BTreeMap::new(),
        phase: js_agent_wire::Phase::Serving,
        agent_process: Some("proc-a".to_owned()),
        agent_version: None,
    };
    let other = &world.jobs;
    let between = || {
        other
            .hello_node_in_pool(&id("host-a"), &theirs, 4, GRACE)
            .expect("the other server's hello is written first");
    };
    let body = Ok(hello_by("host-a", "proc-b"));
    let refused = serve_hello(&server.agents, server.who.clone(), None, body, &between).await;
    let refused = refused.err().expect("refused");
    assert_eq!(said(&refused), (409, "host-live"));
    let row = world.row("host-a");
    assert_eq!(
        (row.record.epoch, row.record.agent_process.as_deref()),
        (1, Some("proc-a"))
    );

    let body = Ok(hello_by("host-a", "proc-a"));
    let welcome = serve_hello(&server.agents, server.who.clone(), None, body, &|| ());
    let Json(welcome) = welcome.await.expect("the row's own process");
    assert_eq!(welcome.epoch, 2);
    assert_eq!(server.holds("host-a"), Some(2));
}
