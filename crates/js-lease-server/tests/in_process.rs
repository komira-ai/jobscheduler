//! The in-process server (`js_lease_server::in_process`, cargo feature `in-process`): the
//! routers answer with no listener, one `tick()` runs the pool checks, the lease table with
//! the agent registry, and the scaler in that order on the injected clock, and the seam ends
//! in the same job and host listings as the tick loop does after the same inputs.
//!
//! Time is the test's clock ([`support::clock`]) and the lease table's timings a clock of
//! the test's own ([`support::Stepping`]); nothing sleeps and nothing measures elapsed time.

// This file is the root of its test binary, so its modules are found by path.
#[path = "in_process/client_tokens.rs"]
mod client_tokens;
#[path = "in_process/elastic.rs"]
mod elastic;
#[path = "in_process/feature.rs"]
mod feature;
#[path = "in_process/jobs.rs"]
mod jobs;
#[path = "in_process/support.rs"]
mod support;

use std::sync::Arc;
use std::time::Duration;

use js_jobs::ScriptedIds;
use js_lease_server::in_process::InProcess;
use js_lease_server::{Cli, Config};
use js_registry::HostState;
use js_state::ManualClock as StoreClock;
use serde_json::{Value, json};

use crate::support::{
    Cell, JOIN, Listening, Reply, Scratch, Stepping, TOKEN, args, beat_body, clock, hello_body,
    routed,
};

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// Asserts that the agent router answers a hello with a welcome that opens a session, and a
/// heartbeat on that session with 200; that the client router does not serve the agent
/// routes (404, as on a server whose agent routes have a port of their own) and does serve
/// its own (`/healthz`, and the hosts list behind the token, which shows the host); and that
/// a status route, with no peer to read, answers as a route that does not exist. It catches
/// a seam that needs a listener to answer, an agent route missing from the agent router, and
/// a status page served to a request with no peer.
#[tokio::test]
async fn an_in_process_server_answers_a_hello_and_a_heartbeat_with_no_socket() {
    let cell = Cell::start("hello", true);
    let welcome = cell.agent("/v1/agent/hello", &hello_body("host-a")).await;
    assert_eq!(welcome.status, 200, "{:?}", welcome.body);
    assert!(
        welcome.body["session"]
            .as_str()
            .is_some_and(|s| !s.is_empty())
    );
    assert_eq!(welcome.body["epoch"], 1);
    let beat = cell
        .agent("/v1/agent/heartbeat", &beat_body("host-a", &welcome))
        .await;
    assert_eq!(beat.status, 200, "{:?}", beat.body);

    let client = cell.server.client_router();
    let elsewhere = routed(&client, "POST", "/v1/agent/hello", Some(TOKEN), None).await;
    assert_eq!(elsewhere.status, 404, "{:?}", elsewhere.body);
    let health = routed(&client, "GET", "/healthz", None, None).await;
    assert_eq!(
        health,
        Reply {
            status: 200,
            body: json!("ok")
        }
    );
    let hosts = cell.call("GET", "/v1/hosts", None).await;
    assert_eq!(hosts.status, 200, "{:?}", hosts.body);
    let listed = hosts.body["hosts"].as_array().unwrap();
    assert!(
        listed.iter().any(|host| host["id"] == "host-a"),
        "{listed:?}"
    );
    let pools = routed(&client, "GET", "/v1/pools", None, None).await;
    assert_eq!(pools.status, 404, "{:?}", pools.body);
    assert_eq!(cell.server.agent_errors(), 0);
}

/// Asserts that a host that said hello reads live through `registry()` while it is inside
/// the grace (15 s, the pool's default) by the injected clock, still live after a tick one
/// second before its end, and lost after the clock passes it and a tick has run. It catches
/// a seam whose registry reads another clock than the one given, and one whose tick does
/// not run the registry or forgets the host.
#[tokio::test]
async fn an_in_process_tick_runs_the_registry_so_a_silent_host_reads_lost() {
    let cell = Cell::start("lost", true);
    let welcome = cell.agent("/v1/agent/hello", &hello_body("host-a")).await;
    assert_eq!(welcome.status, 200, "{:?}", welcome.body);
    let registry = cell.server.registry().unwrap();
    let state = || {
        let hosts = registry.hosts();
        assert_eq!(hosts.len(), 1, "{hosts:?}");
        (
            hosts[0].state,
            hosts[0].lease.map(|lease| lease.to_string()),
        )
    };
    assert_eq!(state(), (HostState::Live, None));

    cell.clock.advance(secs(14));
    cell.server.tick().await;
    assert_eq!(state(), (HostState::Live, None));
    cell.clock.advance(secs(2));
    cell.server.tick().await;
    assert_eq!(state(), (HostState::Lost, None));
    assert_eq!(cell.server.agent_errors(), 0);
}

/// The body of a job `id`: one core and one GiB, as the registered host has room for.
fn job_body(id: &str) -> Value {
    json!({
        "spec": {
            "jobId": id,
            "image": "team/app@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "command": "run",
            "needs": { "cpuMillis": "1000", "memoryBytes": "1073741824" },
            "wallLimitSeconds": "600",
        }
    })
}

/// The inputs both servers of the comparison get, in order.
#[derive(Clone, Copy)]
enum Input {
    /// A submit of the job of this id to the job service.
    Submit(&'static str),
    /// A cancel of the job of this id.
    Cancel(&'static str),
    /// A hello of `host-a`.
    Hello,
    /// A heartbeat of `host-a` on the session of its hello.
    Beat,
    /// The clock moves this many seconds.
    Advance(u64),
    /// One tick, after which the listings are compared.
    Tick,
}

/// The configuration of both servers of the comparison: the job service on, its tables
/// stamped by one fixed store clock and its ids from one fixed source, so that the two
/// servers write equal rows.
fn equal_config(scratch: &Scratch, monotonic: &Stepping) -> Config {
    let mut flags = args(scratch, "initialise", true);
    flags.push("--job-listen".into());
    flags.push("127.0.0.1:0".into());
    let cli = Cli::try_parse_from(flags).unwrap();
    let ids = Arc::new(ScriptedIds::new([1; 16], []));
    Config::from_cli(cli)
        .unwrap()
        .with_monotonic(Arc::new(monotonic.clone()))
        .with_job_sources(Arc::new(StoreClock::default()), ids)
}

/// What a server lists after a tick: its jobs and its hosts.
type Listing = (Reply, Reply);

/// Runs `inputs` on the server the listening way (a loopback port and a `ChannelTicker`) and
/// returns what it lists after each tick.
async fn through_the_tick_loop(inputs: &[Input]) -> Vec<Listing> {
    let scratch = Scratch::new("equal-loop");
    let (clock, shared) = clock();
    let monotonic = Stepping::of(Duration::from_millis(1));
    let server = Listening::start(equal_config(&scratch, &monotonic), shared).await;
    let mut welcome = None;
    let mut seen = Vec::new();
    for input in inputs {
        match *input {
            Input::Submit(id) => {
                let submitted = server
                    .job_request("POST", "/v1/jobs", Some(&job_body(id)))
                    .await;
                assert_eq!(submitted.status, 201, "{:?}", submitted.body);
            }
            Input::Cancel(id) => {
                let path = format!("/v1/jobs/{id}:cancel");
                let cancelled = server.job_request("POST", &path, Some(&json!({}))).await;
                assert_eq!(cancelled.status, 200, "{:?}", cancelled.body);
            }
            Input::Hello => {
                let hello = hello_body("host-a");
                welcome = Some(
                    server
                        .request("POST", "/v1/agent/hello", JOIN, Some(&hello))
                        .await,
                );
            }
            Input::Beat => {
                let beat = beat_body("host-a", welcome.as_ref().unwrap());
                let reply = server
                    .request("POST", "/v1/agent/heartbeat", JOIN, Some(&beat))
                    .await;
                assert_eq!(reply.status, 200, "{:?}", reply.body);
            }
            Input::Advance(n) => clock.advance(secs(n)),
            Input::Tick => {
                server.ticks.step().await;
                let jobs = server.job_request("GET", "/v1/jobs", None).await;
                let hosts = server.request("GET", "/v1/hosts", TOKEN, None).await;
                seen.push((jobs, hosts));
            }
        }
    }
    server.running.shutdown().await.unwrap();
    seen
}

/// Runs `inputs` on the server in process and returns what it lists after each tick.
async fn through_the_seam(inputs: &[Input]) -> Vec<Listing> {
    let scratch = Scratch::new("equal-seam");
    let (clock, shared) = clock();
    let monotonic = Stepping::of(Duration::from_millis(1));
    let server = InProcess::start(equal_config(&scratch, &monotonic), shared).unwrap();
    let agents = server.agent_router();
    let jobs = server.job_router().expect("the job service is on");
    let client = server.client_router();
    let mut welcome = None;
    let mut seen = Vec::new();
    for input in inputs {
        match *input {
            Input::Submit(id) => {
                let body = job_body(id);
                let submitted = routed(&jobs, "POST", "/v1/jobs", Some(TOKEN), Some(&body)).await;
                assert_eq!(submitted.status, 201, "{:?}", submitted.body);
            }
            Input::Cancel(id) => {
                let path = format!("/v1/jobs/{id}:cancel");
                let body = json!({});
                let cancelled = routed(&jobs, "POST", &path, Some(TOKEN), Some(&body)).await;
                assert_eq!(cancelled.status, 200, "{:?}", cancelled.body);
            }
            Input::Hello => {
                let hello = hello_body("host-a");
                let path = "/v1/agent/hello";
                welcome = Some(routed(&agents, "POST", path, Some(JOIN), Some(&hello)).await);
            }
            Input::Beat => {
                let beat = beat_body("host-a", welcome.as_ref().unwrap());
                let path = "/v1/agent/heartbeat";
                let reply = routed(&agents, "POST", path, Some(JOIN), Some(&beat)).await;
                assert_eq!(reply.status, 200, "{:?}", reply.body);
            }
            Input::Advance(n) => clock.advance(secs(n)),
            Input::Tick => {
                server.tick().await;
                let listed = routed(&jobs, "GET", "/v1/jobs", Some(TOKEN), None).await;
                let hosts = routed(&client, "GET", "/v1/hosts", Some(TOKEN), None).await;
                seen.push((listed, hosts));
            }
        }
    }
    seen
}

/// Asserts that the tick loop and the seam, over equal (fresh) state files, equal clocks and
/// equal inputs, list equal jobs and equal hosts after every one of five ticks. The inputs
/// register a host, submit two jobs, cancel one, and let the registered host go silent past
/// its grace, so the listings change at every tick: a job is queued, a second one joins it,
/// the first is cancelled, and the host goes from live to lost. It catches a seam whose tick
/// does less, more, or another thing than the tick loop's (a part of `Shared::tick` skipped
/// or done twice), and routers that answer otherwise than the listeners.
#[tokio::test]
async fn the_tick_loop_and_the_seam_leave_equal_job_listings_after_equal_ticks() {
    use Input::{Advance, Beat, Cancel, Hello, Submit, Tick};
    let inputs = [
        Hello,
        Submit("job-a"),
        Tick,
        Submit("job-b"),
        Advance(10),
        Beat,
        Tick,
        Cancel("job-a"),
        Tick,
        Advance(11),
        Beat,
        Tick,
        Advance(20),
        Tick,
    ];
    let looped = through_the_tick_loop(&inputs).await;
    let seamed = through_the_seam(&inputs).await;
    assert_eq!(looped.len(), 5);
    let jobs = |seen: &[Listing]| {
        seen.iter()
            .map(|(jobs, _)| {
                assert_eq!(jobs.status, 200, "{:?}", jobs.body);
                let listed = jobs.body["jobs"].as_array().unwrap();
                let state = |job: &Value| {
                    let id = job["id"].as_str().unwrap().to_owned();
                    (id, job["state"].as_str().unwrap().to_owned())
                };
                listed.iter().map(state).collect::<Vec<_>>()
            })
            .collect::<Vec<_>>()
    };
    // What the listings hold, so that equal listings are not equal for being empty.
    let queued = |id: &str| (id.to_owned(), "JOB_STATE_QUEUED".to_owned());
    let cancelled = |id: &str| (id.to_owned(), "JOB_STATE_CANCELLED".to_owned());
    let expected = vec![
        vec![queued("job-a")],
        vec![queued("job-a"), queued("job-b")],
        vec![cancelled("job-a"), queued("job-b")],
        vec![cancelled("job-a"), queued("job-b")],
        vec![cancelled("job-a"), queued("job-b")],
    ];
    assert_eq!(jobs(&looped), expected);
    let states: Vec<String> = looped
        .iter()
        .map(|(_, hosts)| {
            let listed = hosts.body["hosts"].as_array().unwrap();
            let host = listed.iter().find(|host| host["id"] == "host-a").unwrap();
            host["state"].as_str().unwrap().to_owned()
        })
        .collect();
    assert_eq!(states, ["live", "live", "live", "live", "lost"]);
    assert_eq!(seamed, looped);
}

/// Asserts that a job survives a restart through the seam: after `shutdown`, a second
/// server over the same state file with `--boot resume` lists it. Before that, a second
/// server is refused naming the state file's job tables while the first still holds them, and again
/// while a clone of the first's router outlives the shut-down handle (the clone holds the
/// tables, as the module doc says), so the start after the clone is dropped is one the lock
/// would have refused. It catches a `shutdown` that leaves the lock on the state file held,
/// and job tables the seam does not write.
#[tokio::test]
async fn a_second_in_process_server_starts_over_the_state_file_after_the_first_is_shut_down() {
    // The job tables are the first of the state file's files a second server takes.
    const HELD: &str = "--job-listen: state file";
    let scratch = Scratch::new("restart");
    let monotonic = Stepping::of(Duration::from_millis(1));
    let start = |boot: &str| {
        let mut flags = args(&scratch, boot, true);
        flags.push("--job-listen".into());
        flags.push("127.0.0.1:0".into());
        // The usage file has its own lock, which would be the one a second server names.
        flags.push("--usage-off".into());
        let cli = Cli::try_parse_from(flags).unwrap();
        let config = Config::from_cli(cli)
            .unwrap()
            .with_monotonic(Arc::new(monotonic.clone()));
        let (_clock, shared) = clock();
        InProcess::start(config, shared)
    };
    let first = start("initialise").unwrap();
    let hello = routed(
        &first.agent_router(),
        "POST",
        "/v1/agent/hello",
        Some(JOIN),
        Some(&hello_body("host-a")),
    )
    .await;
    assert_eq!(hello.status, 200, "{:?}", hello.body);
    let jobs = first.job_router().expect("the job service is on");
    let body = job_body("job-kept");
    let submitted = routed(&jobs, "POST", "/v1/jobs", Some(TOKEN), Some(&body)).await;
    assert_eq!(submitted.status, 201, "{:?}", submitted.body);

    let refused = start("resume")
        .err()
        .expect("the first server holds the state file");
    assert!(refused.to_string().starts_with(HELD), "{refused}");
    first.shutdown();
    let still = start("resume")
        .err()
        .expect("the router's clone holds the state file");
    assert!(still.to_string().starts_with(HELD), "{still}");
    drop(jobs);

    let second = start("resume").unwrap();
    let jobs = second.job_router().expect("the job service is on");
    let listed = routed(&jobs, "GET", "/v1/jobs", Some(TOKEN), None).await;
    assert_eq!(listed.status, 200, "{:?}", listed.body);
    assert_eq!(listed.body["jobs"][0]["id"], "job-kept");
    assert_eq!(listed.body["jobs"][0]["state"], "JOB_STATE_QUEUED");
}
