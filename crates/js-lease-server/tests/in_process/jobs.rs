//! The job service through the seam: [`InProcess::job_router`] serves the job service's
//! port with no listener, [`InProcess::jobs`] reads the tables it serves from, and
//! [`Config::with_job_sources`] decides the clock the tables stamp their rows with and the
//! source their ids are minted from.

use std::sync::Arc;
use std::time::Duration;

use js_callback::{CallbackAnswer, FailureClass, SigningKey};
use js_jobs::{
    CallbackClaim, CallbackKey, CallbackState, JobCallback, JobId, JobSpec, JobState, NewJob,
    ScriptedIds,
};
use js_lease_server::in_process::InProcess;
use js_lease_server::{Cli, Config};
use js_state::{ManualClock as StoreClock, StoreTime};
use serde_json::json;

use crate::support::{JOIN, Scratch, Stepping, TOKEN, args, clock, hello_body, routed};

/// The image every job here runs: pinned by digest, as the service demands.
const IMAGE: &str =
    "team/app@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

/// The configuration of [`args`], with the job service on (`--job-listen` on the loopback,
/// never bound in process) when `jobs`.
fn job_config(scratch: &Scratch, jobs: bool) -> Config {
    let mut args = args(scratch, "initialise");
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
/// clock's time (42 s) and still queued (`JOB_STATE_QUEUED`, no host: a submit only queues
/// the job and pokes the placement, which this server leaves to its next tick); that after
/// one tick the registered host, having room, holds it (`JOB_STATE_PLACED`, host `host-a`,
/// placed at the same time); and that the tables `jobs()` read hold that job, placed,
/// stamped with the same time; and that the router
/// refuses a call without the bearer token. It catches a seam that mints from the system's
/// random source or stamps with the system's clock whatever it is given (for the queueing or
/// the placement), a job router that is not the one the tables serve, a submit that leaves a
/// job queued over a host with room after the tick, and one served without the token.
#[tokio::test]
async fn the_job_service_is_served_through_the_seam_on_the_given_clock_and_ids() {
    let scratch = Scratch::new("jobs-seam");
    let (_clock, shared) = clock();
    let store_clock = Arc::new(StoreClock::default());
    store_clock.advance(Duration::from_secs(42));
    let ids = Arc::new(ScriptedIds::new([7; 16], []));
    let config = job_config(&scratch, true).with_job_sources(store_clock, ids);
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
    assert!(
        submitted.body["job"]["hostId"].is_null(),
        "{:?}",
        submitted.body
    );
    assert_eq!(submitted.body["job"]["times"]["queuedAtMs"], "42000");

    server.tick().await;
    let path = format!("/v1/jobs/{minted}");
    let placed = routed(&router, "GET", &path, Some(TOKEN), None).await;
    assert_eq!(placed.status, 200, "{:?}", placed.body);
    assert_eq!(placed.body["job"]["state"], "JOB_STATE_PLACED");
    assert_eq!(placed.body["job"]["hostId"], "host-a");
    assert_eq!(placed.body["job"]["times"]["queuedAtMs"], "42000");
    assert_eq!(placed.body["job"]["times"]["placedAtMs"], "42000");

    let jobs = server.jobs().expect("the job tables are open");
    let id = JobId::parse(&minted).unwrap();
    let record = jobs.get_job(&id).unwrap().expect("the job is stored");
    assert_eq!(record.state, JobState::Placed);
    assert_eq!(
        record.submitted_at,
        StoreTime::from_duration(Duration::from_secs(42))
    );

    let refused = routed(&router, "GET", &path, None, None).await;
    assert_eq!(refused.status, 401, "{:?}", refused.body);
}

/// Asserts that the seam's poll runs the placement poller's turn the server would run on a task
/// of its own: before any submit it has no poke and does nothing; after a submit, answered
/// `JOB_STATE_QUEUED`, it takes the submit's poke and places the job on the host with room, with
/// no tick; and a second poll finds no poke left. It catches a seam whose submit cannot be
/// placed before a tick, a poll that runs a pass without a poke, and a poke kept after its pass.
#[tokio::test]
async fn a_poll_after_a_submit_places_the_job_with_no_tick() {
    let scratch = Scratch::new("jobs-seam-poll");
    let (_clock, shared) = clock();
    let server = InProcess::start(job_config(&scratch, true), shared).unwrap();
    let hello = routed(
        &server.agent_router(),
        "POST",
        "/v1/agent/hello",
        Some(JOIN),
        Some(&hello_body("host-a")),
    )
    .await;
    assert_eq!(hello.status, 200, "{:?}", hello.body);
    assert!(!server.poll().await);

    let router = server.job_router().expect("the job service is on");
    let spec = json!({
        "spec": {
            "jobId": "job-p",
            "image": IMAGE,
            "needs": { "cpuMillis": "1000", "memoryBytes": "1073741824" },
            "wallLimitSeconds": "60",
        }
    });
    let submitted = routed(&router, "POST", "/v1/jobs", Some(TOKEN), Some(&spec)).await;
    assert_eq!(submitted.status, 201, "{:?}", submitted.body);
    assert_eq!(submitted.body["job"]["state"], "JOB_STATE_QUEUED");
    assert!(server.poll().await);
    let read = routed(&router, "GET", "/v1/jobs/job-p", Some(TOKEN), None).await;
    assert_eq!(
        (&read.body["job"]["state"], &read.body["job"]["hostId"]),
        (&json!("JOB_STATE_PLACED"), &json!("host-a")),
        "{:?}",
        read.body
    );
    assert!(!server.poll().await);
}

/// Asserts that a server with no `--job-listen`, with and without job sources given (which
/// then go to nothing), starts through the seam with its registry and no job router and no
/// job tables. It catches a seam that opens the job tables, or serves the job service, when
/// the configuration turned it off, and a `with_job_sources` that fails a server without
/// the job service.
#[tokio::test]
async fn without_the_job_service_the_seam_has_no_job_router_and_no_tables() {
    let registering = Scratch::new("jobs-off");
    let (_clock, shared) = clock();
    let server = InProcess::start(job_config(&registering, false), shared).unwrap();
    assert!(server.registry().is_some());
    assert!(server.job_router().is_none());
    assert!(server.jobs().is_none());

    let sourced = Scratch::new("jobs-off-sourced");
    let (_clock, shared) = clock();
    let ids = Arc::new(ScriptedIds::new([1; 16], []));
    let config = job_config(&sourced, false).with_job_sources(Arc::new(StoreClock::default()), ids);
    let server = InProcess::start(config, shared).unwrap();
    assert!(server.registry().is_some());
    assert!(server.job_router().is_none());
    assert!(server.jobs().is_none());
}

/// Asserts that a server started from its flags (`--job-listen` given) refuses a job that asks
/// for a completion callback, `400` with exactly `spec.callback: callback-not-enabled`, and
/// records nothing: this release takes no callback, whatever the callback holds, until the
/// release that accepts them. It catches a start that turns the refusal off.
#[tokio::test]
async fn a_server_from_its_flags_refuses_a_callback() {
    let scratch = Scratch::new("jobs-seam-callback");
    let (_clock, shared) = clock();
    let server = InProcess::start(job_config(&scratch, true), shared).unwrap();
    let router = server.job_router().expect("the job service is on");
    let spec = json!({
        "spec": {
            "jobId": "job-cb",
            "image": IMAGE,
            "needs": { "cpuMillis": "1000", "memoryBytes": "1073741824" },
            "wallLimitSeconds": "60",
            "callback": {
                "url": "https://hooks.example/done",
                "signingKey": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
            },
        }
    });
    let refused = routed(&router, "POST", "/v1/jobs", Some(TOKEN), Some(&spec)).await;
    assert_eq!(refused.status, 400, "{:?}", refused.body);
    assert_eq!(
        refused.body["field"], "spec.callback: callback-not-enabled",
        "{:?}",
        refused.body
    );
    let jobs = server.jobs().expect("the job tables are open");
    let id = JobId::parse("job-cb").unwrap();
    assert_eq!(jobs.get_job(&id).unwrap(), None);
}

/// The body of a submit of job `id`, with the retry policy and the callback it is given.
fn submit_body(id: &str, retry: bool, callback: Option<&str>) -> serde_json::Value {
    let mut spec = json!({
        "jobId": id,
        "image": IMAGE,
        "needs": { "cpuMillis": "1000", "memoryBytes": "1073741824" },
        "wallLimitSeconds": "60",
    });
    if retry {
        spec["retry"] = json!({ "maxAttempts": 3 });
    }
    if let Some(url) = callback {
        let key = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
        spec["callback"] = json!({ "url": url, "signingKey": key });
    }
    json!({ "spec": spec })
}

/// A server on `config` with host `host-a` registered, the job service's router, and the store
/// clock its tables stamp with (at 42 s).
async fn served(config: Config) -> (InProcess, axum::Router, Arc<StoreClock>) {
    let (_clock, shared) = clock();
    let store_clock = Arc::new(StoreClock::default());
    store_clock.advance(Duration::from_secs(42));
    let ids = Arc::new(ScriptedIds::new([7; 16], []));
    let config = config.with_job_sources(store_clock.clone(), ids);
    let server = InProcess::start(config, shared).unwrap();
    let agents = server.agent_router();
    let body = hello_body("host-a");
    let hello = routed(&agents, "POST", "/v1/agent/hello", Some(JOIN), Some(&body)).await;
    assert_eq!(hello.status, 200, "{:?}", hello.body);
    let router = server.job_router().expect("the job service is on");
    (server, router, store_clock)
}

/// Asserts that a server built from its flags refuses a job's retry policy, `400` with exactly
/// `spec.retry: retry-not-enabled`, and its completion callback (`spec.callback:
/// callback-not-enabled`), and that the same configuration passed through
/// `accepting_retry_and_callbacks` answers `201` to each and keeps each; and that the call on a
/// configuration with no job service changes nothing. It catches a lift that opens only one of
/// the two, a flags' server that takes either, and a lift that turns a job service on.
#[tokio::test]
async fn only_a_config_told_to_accept_takes_a_retry_policy_and_a_callback() {
    let url = Some("https://hooks.example/done");
    for accepting in [false, true] {
        let scratch = Scratch::new("jobs-seam-accepting");
        let config = job_config(&scratch, true);
        let config = if accepting {
            config.accepting_retry_and_callbacks()
        } else {
            config
        };
        let (server, router, _) = served(config).await;
        let retry = submit_body("job-r", true, None);
        let retried = routed(&router, "POST", "/v1/jobs", Some(TOKEN), Some(&retry)).await;
        let callback = submit_body("job-c", false, url);
        let called = routed(&router, "POST", "/v1/jobs", Some(TOKEN), Some(&callback)).await;
        let jobs = server.jobs().expect("the job tables are open");
        let (retry_id, callback_id) = (JobId::parse("job-r"), JobId::parse("job-c"));
        let (retry_id, callback_id) = (retry_id.unwrap(), callback_id.unwrap());
        if accepting {
            assert_eq!(retried.status, 201, "{:?}", retried.body);
            assert_eq!(called.status, 201, "{:?}", called.body);
            let kept = jobs.get_job(&retry_id).unwrap().expect("the job is stored");
            assert_eq!(kept.spec.retry.map(|policy| policy.max_attempts), Some(3));
            let kept = jobs
                .get_job(&callback_id)
                .unwrap()
                .expect("the job is stored");
            assert_eq!(kept.spec.callback.map(|c| c.url).as_deref(), url);
        } else {
            assert_eq!(retried.status, 400, "{:?}", retried.body);
            assert_eq!(retried.body["field"], "spec.retry: retry-not-enabled");
            assert_eq!(called.status, 400, "{:?}", called.body);
            assert_eq!(called.body["field"], "spec.callback: callback-not-enabled");
            assert_eq!(jobs.get_job(&retry_id).unwrap(), None);
            assert_eq!(jobs.get_job(&callback_id).unwrap(), None);
        }
    }

    let scratch = Scratch::new("jobs-seam-accepting-off");
    let (_clock, shared) = clock();
    let config = job_config(&scratch, false).accepting_retry_and_callbacks();
    let server = InProcess::start(config, shared).unwrap();
    assert!(server.job_router().is_none());
    assert!(server.jobs().is_none());
}

/// Records job `id` through the router with a callback to `url`, as the accepting server takes
/// it, and ends it by a cancel at the tables, so its callback is due.
async fn ended_with_callback(server: &InProcess, router: &axum::Router, id: &str, url: &str) {
    let body = submit_body(id, false, Some(url));
    let made = routed(router, "POST", "/v1/jobs", Some(TOKEN), Some(&body)).await;
    assert_eq!(made.status, 201, "{:?}", made.body);
    let jobs = server.jobs().expect("the job tables are open");
    jobs.request_cancel(&JobId::parse(id).unwrap()).unwrap();
}

/// Asserts that `deliver_callback` is one try of the delivery a server's threads make: with
/// nothing due it claims nothing and never calls `send`; with a cancelled job's callback due
/// it hands `send` the job, its webhook id and the claim's instant in whole seconds (47, the
/// store clock after it was advanced), and a `Failed` answer leaves the callback `Pending`
/// with one try counted and nothing due at once; after the wait the next try carries the same
/// webhook id and the next instant, and a 2xx answer records `Delivered` with two tries, after
/// which a call claims nothing. It catches a seam that does not record the answer it is given,
/// one that tries a callback twice or not at all, a timestamp read from another clock, and a
/// webhook id that changes between tries.
#[tokio::test]
async fn deliver_callback_tries_one_due_callback_and_records_its_answer() {
    let scratch = Scratch::new("jobs-seam-deliver");
    let config = job_config(&scratch, true).accepting_retry_and_callbacks();
    let (server, router, clock) = served(config).await;
    let mut never = |_: &CallbackClaim, _: u64| -> CallbackAnswer { panic!("nothing is due") };
    assert!(!server.deliver_callback("sim-server", &mut never));

    ended_with_callback(&server, &router, "job-d", "https://hooks.example/done").await;
    clock.advance(Duration::from_secs(5));
    let id = JobId::parse("job-d").unwrap();
    let jobs = server.jobs().expect("the job tables are open");
    let mut seen = Vec::new();
    let mut failing = |claim: &CallbackClaim, at: u64| {
        seen.push((claim.job.clone(), claim.webhook_id.clone(), at));
        CallbackAnswer::Failed(FailureClass::Failed)
    };
    assert!(server.deliver_callback("sim-server", &mut failing));
    let status = jobs.callback_status(&id).unwrap().expect("a callback");
    assert_eq!((status.state, status.tries), (CallbackState::Pending, 1));
    assert!(!server.deliver_callback("sim-server", &mut never));

    clock.advance(Duration::from_secs(3600));
    let mut delivering = |claim: &CallbackClaim, at: u64| {
        seen.push((claim.job.clone(), claim.webhook_id.clone(), at));
        CallbackAnswer::Delivered
    };
    assert!(server.deliver_callback("sim-server", &mut delivering));
    let status = jobs.callback_status(&id).unwrap().expect("a callback");
    assert_eq!((status.state, status.tries), (CallbackState::Delivered, 2));
    assert!(!server.deliver_callback("sim-server", &mut never));
    let [first, second] = seen.as_slice() else {
        panic!("two tries were made: {seen:?}");
    };
    assert_eq!((&first.0, first.2), (&id, 47));
    assert_eq!((&second.0, second.2), (&id, 47 + 3600));
    assert!(!first.1.is_empty());
    assert_eq!(first.1, second.1);
}

/// Asserts that a callback whose URL the build refuses (a row written past submit's checks:
/// plain `http`) is never handed to `send`, but its try is counted and fails (state `Pending`,
/// one try), and that a server whose job service is off claims nothing and never calls `send`.
/// It catches a seam that sends a URL the sender would refuse, and one that does not run the
/// job service's own delivery path.
#[tokio::test]
async fn deliver_callback_never_sends_a_url_the_build_refuses_and_claims_nothing_when_off() {
    let scratch = Scratch::new("jobs-seam-refused");
    let config = job_config(&scratch, true).accepting_retry_and_callbacks();
    let (server, router, _) = served(config).await;
    ended_with_callback(&server, &router, "job-ok", "https://hooks.example/done").await;
    let jobs = server.jobs().expect("the job tables are open");
    let base = jobs.get_job(&JobId::parse("job-ok").unwrap()).unwrap();
    let key = SigningKey::new(vec![1; 32]).unwrap();
    let callback = JobCallback {
        url: "http://hooks.example/done".to_owned(),
        key: CallbackKey::Key(key),
        deadline_seconds: Some(3600),
    };
    let spec = JobSpec {
        callback: Some(callback),
        ..base.expect("the job is stored").spec
    };
    let id = JobId::parse("job-bad").unwrap();
    let new = NewJob {
        id: Some(id.clone()),
        spec,
        priority: 0,
        client: None,
    };
    jobs.insert_queued(new).unwrap();
    jobs.request_cancel(&id).unwrap();

    let sent = std::cell::RefCell::new(Vec::new());
    let mut send = |claim: &CallbackClaim, _: u64| {
        sent.borrow_mut().push(claim.job.clone());
        CallbackAnswer::Delivered
    };
    // Both callbacks are due; the good one is sent, the bad one's try fails unsent.
    assert!(server.deliver_callback("sim-server", &mut send));
    assert!(server.deliver_callback("sim-server", &mut send));
    assert!(!server.deliver_callback("sim-server", &mut send));
    assert_eq!(*sent.borrow(), vec![JobId::parse("job-ok").unwrap()]);
    let status = jobs.callback_status(&id).unwrap().expect("a callback");
    assert_eq!((status.state, status.tries), (CallbackState::Pending, 1));

    let off = Scratch::new("jobs-seam-deliver-off");
    let (_clock, shared) = clock();
    let server = InProcess::start(job_config(&off, false), shared).unwrap();
    assert!(!server.deliver_callback("sim-server", &mut send));
    assert_eq!(sent.borrow().len(), 1);
}
