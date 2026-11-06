//! The job service through the seam: [`InProcess::job_router`] serves the job service's
//! port with no listener, [`InProcess::jobs`] reads the tables it serves from, and
//! [`Config::with_job_sources`] decides the clock the tables stamp their rows with and the
//! source their ids are minted from.

use std::sync::Arc;
use std::time::Duration;

use js_jobs::{JobId, JobState, ScriptedIds};
use js_lease_server::in_process::InProcess;
use js_lease_server::{Cli, Config};
use js_state::{ManualClock as StoreClock, StoreTime};
use serde_json::json;

use crate::support::{JOIN, Scratch, Stepping, TOKEN, args, clock, hello_body, routed};

/// The image every job here runs: pinned by digest, as the service demands.
const IMAGE: &str =
    "team/app@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

/// The configuration of [`args`] (with agent registration when `agents`), with the job
/// service on (`--job-listen` on the loopback, never bound in process) when `jobs`.
fn job_config(scratch: &Scratch, agents: bool, jobs: bool) -> Config {
    let mut args = args(scratch, "initialise", agents);
    if jobs {
        args.push("--job-listen".into());
        args.push("127.0.0.1:0".into());
    }
    let cli = Cli::try_parse_from(args).unwrap();
    let monotonic = Stepping::of(Duration::from_millis(1));
    Config::from_cli(cli)
        .unwrap()
        .with_monotonic(Arc::new(monotonic))
}

/// Asserts that a job submitted without an id through the job router is answered 201 under
/// the id the given source minted (its bits as 32 hex digits), queued at the given store
/// clock's time (42 s), and that the tables `jobs()` reads hold that job, queued, stamped
/// with the same time; and that the router refuses a call without the bearer token. It
/// catches a seam that mints from the system's random source or stamps with the system's
/// clock whatever it is given, a job router that is not the one the tables serve, and one
/// served without the token.
#[tokio::test]
async fn the_job_service_is_served_through_the_seam_on_the_given_clock_and_ids() {
    let scratch = Scratch::new("jobs-seam");
    let (_clock, shared) = clock();
    let store_clock = Arc::new(StoreClock::default());
    store_clock.advance(Duration::from_secs(42));
    let ids = Arc::new(ScriptedIds::new([7; 16], []));
    let config = job_config(&scratch, true, true).with_job_sources(store_clock, ids);
    let server = InProcess::start(config, shared).unwrap();
    let agents = server.agent_router();
    let hello = routed(
        &agents,
        "POST",
        "/v1/agent/hello",
        Some(JOIN),
        Some(&hello_body("host-a")),
    )
    .await;
    assert_eq!(hello.status, 200, "{:?}", hello.body);

    let router = server.job_router().expect("the job service is on");
    let spec = json!({
        "spec": {
            "image": IMAGE,
            "needs": { "cpuMillis": "1000", "memoryBytes": "1073741824" },
            "wallLimitSeconds": "60",
        }
    });
    let submitted = routed(&router, "POST", "/v1/jobs", Some(TOKEN), Some(&spec)).await;
    assert_eq!(submitted.status, 201, "{:?}", submitted.body);
    let minted = "07".repeat(16);
    assert_eq!(submitted.body["job"]["id"], minted.as_str());
    assert_eq!(submitted.body["job"]["state"], "JOB_STATE_QUEUED");
    assert_eq!(submitted.body["job"]["times"]["queuedAtMs"], "42000");

    let jobs = server.jobs().expect("the job tables are open");
    let id = JobId::parse(&minted).unwrap();
    let record = jobs.get_job(&id).unwrap().expect("the job is stored");
    assert_eq!(record.state, JobState::Queued);
    assert_eq!(
        record.submitted_at,
        StoreTime::from_duration(Duration::from_secs(42))
    );

    let path = format!("/v1/jobs/{minted}");
    let refused = routed(&router, "GET", &path, None, None).await;
    assert_eq!(refused.status, 401, "{:?}", refused.body);
}

/// Asserts that a server with agent registration and no `--job-listen`, and one with no
/// agent registration at all (whose job sources are then given to nothing), start through
/// the seam with no job router and no job tables. It catches a seam that opens the job
/// tables, or serves the job service, when the configuration turned it off, and a
/// `with_job_sources` that fails a server with no agent side.
#[tokio::test]
async fn without_the_job_service_the_seam_has_no_job_router_and_no_tables() {
    let registering = Scratch::new("jobs-off");
    let (_clock, shared) = clock();
    let server = InProcess::start(job_config(&registering, true, false), shared).unwrap();
    assert!(server.registry().is_some());
    assert!(server.job_router().is_none());
    assert!(server.jobs().is_none());

    let fixed = Scratch::new("jobs-static");
    let (_clock, shared) = clock();
    let ids = Arc::new(ScriptedIds::new([1; 16], []));
    let config =
        job_config(&fixed, false, false).with_job_sources(Arc::new(StoreClock::default()), ids);
    let server = InProcess::start(config, shared).unwrap();
    assert!(server.registry().is_none());
    assert!(server.job_router().is_none());
    assert!(server.jobs().is_none());
}
