//! The in-process server (`js_lease_server::in_process`, cargo feature `in-process`): the
//! routers answer with no listener, one `tick()` runs the pool checks, the lease table with
//! the agent registry, and the scaler in that order on the injected clock, and the seam ends
//! in the same lease and host listings as the tick loop does after the same inputs.
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

use std::time::Duration;

use js_lease_server::in_process::InProcess;
use js_registry::HostState;
use serde_json::json;

use crate::support::{
    Cell, JOIN, Listening, Reply, Scratch, Stepping, T0, TOKEN, beat_body, clock, config,
    hello_body, ids, lease_body, lease_body_with_wall, routed,
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

/// Asserts that a lease granted through the client router ends at its wall end by the
/// injected clock and only at a tick: one second before its end a tick keeps it, past its end
/// it is still listed until `tick()`, and the tick frees it; that no tick failed to write the
/// lease file; and that the table's steps were timed on the monotonic clock the caller gave
/// (each takes exactly its 1500 ms step, which no real clock would give). It catches a seam
/// that reads another clock than the one given, one whose `tick()` does not tick the table,
/// and one that times the table on the system's clock.
#[tokio::test]
async fn an_in_process_tick_ends_a_lease_past_its_wall_end_on_the_injected_clock() {
    let scratch = Scratch::new("expire");
    let (clock, shared) = clock();
    let monotonic = Stepping::of(Duration::from_millis(1500));
    let server =
        InProcess::start(config(&scratch, "initialise", false, &monotonic), shared).unwrap();
    let client = server.client_router();
    let granted = routed(
        &client,
        "POST",
        "/v1/leases",
        Some(TOKEN),
        Some(&lease_body_with_wall(60)),
    )
    .await;
    assert_eq!(granted.status, 201, "{:?}", granted.body);
    assert_eq!(granted.body["ends_at_ms"], T0 + 60_000);
    let list = || routed(&client, "GET", "/v1/leases", Some(TOKEN), None);

    clock.advance(secs(59));
    server.tick().await;
    assert_eq!(ids(&list().await), vec!["1-1"]);
    clock.advance(secs(2));
    assert_eq!(ids(&list().await), vec!["1-1"]);
    server.tick().await;
    assert!(ids(&list().await).is_empty());
    assert_eq!(server.save_errors(), 0);
    let report = server.timings().report();
    assert!(
        report.contains("wait-acquire: 1 timed, 1 over 1 s, longest 1500 ms"),
        "{report}"
    );
    assert!(server.registry().is_none());
    assert!(server.scaler().is_none());
}

/// Asserts that a host that said hello reads live through `registry()` while it is inside
/// the grace (15 s, the pool's default) by the injected clock, still live after a tick one
/// second before its end, and lost after the clock passes it and a tick has run; and that the
/// lease placed on it stays booked to it (a lost host keeps its booking). It catches a seam
/// whose registry reads another clock than the one given, and one whose tick drops the lease
/// of a silent host or forgets the host.
#[tokio::test]
async fn an_in_process_tick_runs_the_registry_so_a_silent_host_reads_lost() {
    let cell = Cell::start("lost", true);
    let welcome = cell.agent("/v1/agent/hello", &hello_body("host-a")).await;
    assert_eq!(welcome.status, 200, "{:?}", welcome.body);
    // The shipped pool's two hosts first, then the registered one.
    for _ in 0..3 {
        let granted = cell.call("POST", "/v1/leases", Some(&lease_body())).await;
        assert_eq!(granted.status, 201, "{:?}", granted.body);
    }
    let registry = cell.server.registry().unwrap();
    let state = || {
        let hosts = registry.hosts();
        assert_eq!(hosts.len(), 1, "{hosts:?}");
        (
            hosts[0].state,
            hosts[0].lease.map(|lease| lease.to_string()),
        )
    };
    assert_eq!(state(), (HostState::Live, Some("1-3".to_owned())));

    cell.clock.advance(secs(14));
    cell.server.tick().await;
    assert_eq!(state(), (HostState::Live, Some("1-3".to_owned())));
    cell.clock.advance(secs(2));
    cell.server.tick().await;
    assert_eq!(state(), (HostState::Lost, Some("1-3".to_owned())));
    assert_eq!(cell.server.agent_errors(), 0);
}

/// A lease request with a maximum wall time of 60 s.
fn wall_of_60() -> serde_json::Value {
    lease_body_with_wall(60)
}

/// A lease request with a maximum wall time of 70 s.
fn wall_of_70() -> serde_json::Value {
    lease_body_with_wall(70)
}

/// The inputs both servers of the comparison get, in order.
#[derive(Clone, Copy)]
enum Input {
    /// A client request, with the client token.
    Client(
        &'static str,
        &'static str,
        Option<fn() -> serde_json::Value>,
    ),
    /// A hello of `host-a`.
    Hello,
    /// A heartbeat of `host-a` on the session of its hello.
    Beat,
    /// The clock moves this many seconds.
    Advance(u64),
    /// One tick, after which the listings are compared.
    Tick,
}

/// What a server lists after a tick: its leases and its hosts.
type Listing = (Reply, Reply);

/// Runs `inputs` on the server the listening way (a loopback port and a `ChannelTicker`) and
/// returns what it lists after each tick.
async fn through_the_tick_loop(inputs: &[Input]) -> Vec<Listing> {
    let scratch = Scratch::new("equal-loop");
    let (clock, shared) = clock();
    let monotonic = Stepping::of(Duration::from_millis(1));
    let server = Listening::start(config(&scratch, "initialise", true, &monotonic), shared).await;
    let mut welcome = None;
    let mut seen = Vec::new();
    for input in inputs {
        match *input {
            Input::Client(method, path, body) => {
                let body = body.map(|body| body());
                server.request(method, path, TOKEN, body.as_ref()).await;
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
                let leases = server.request("GET", "/v1/leases", TOKEN, None).await;
                let hosts = server.request("GET", "/v1/hosts", TOKEN, None).await;
                seen.push((leases, hosts));
            }
        }
    }
    server.running.shutdown().await.unwrap();
    seen
}

/// Runs `inputs` on the server in process and returns what it lists after each tick.
async fn through_the_seam(inputs: &[Input]) -> Vec<Listing> {
    let cell = Cell::start("equal-seam", true);
    let mut welcome = None;
    let mut seen = Vec::new();
    for input in inputs {
        match *input {
            Input::Client(method, path, body) => {
                let body = body.map(|body| body());
                cell.call(method, path, body.as_ref()).await;
            }
            Input::Hello => {
                welcome = Some(cell.agent("/v1/agent/hello", &hello_body("host-a")).await)
            }
            Input::Beat => {
                let beat = beat_body("host-a", welcome.as_ref().unwrap());
                let reply = cell.agent("/v1/agent/heartbeat", &beat).await;
                assert_eq!(reply.status, 200, "{:?}", reply.body);
            }
            Input::Advance(n) => cell.clock.advance(secs(n)),
            Input::Tick => {
                cell.server.tick().await;
                let leases = cell.call("GET", "/v1/leases", None).await;
                let hosts = cell.call("GET", "/v1/hosts", None).await;
                seen.push((leases, hosts));
            }
        }
    }
    seen
}

/// Asserts that the tick loop and the seam, over equal (fresh) lease files, equal clocks and
/// equal inputs, list equal leases and equal hosts after every one of five ticks. The inputs
/// fill the shipped pool's two hosts and the registered one (one lease with a longer wall
/// time than the others), let the others end, lease a freed host again, let the longer
/// lease end too, and let the registered host go silent past its grace, so the listings
/// change at every tick: a lease is placed, ended and freed, and the host goes from live to lost. It catches a seam whose tick does less, more,
/// or another thing than the tick loop's (a part of `Shared::tick` skipped or done twice),
/// and routers that answer otherwise than the listener.
#[tokio::test]
async fn the_tick_loop_and_the_seam_leave_equal_lease_listings_after_equal_ticks() {
    use Input::{Advance, Beat, Client, Hello, Tick};
    let acquire = Client("POST", "/v1/leases", Some(wall_of_60));
    let longer = Client("POST", "/v1/leases", Some(wall_of_70));
    let inputs = [
        Hello,
        longer,
        acquire,
        Tick,
        acquire,
        Advance(10),
        Beat,
        Tick,
        Advance(40),
        Beat,
        Tick,
        Advance(11),
        Beat,
        Tick,
        acquire,
        Advance(20),
        Tick,
    ];
    let looped = through_the_tick_loop(&inputs).await;
    let seamed = through_the_seam(&inputs).await;
    assert_eq!(looped.len(), 5);
    let leases = |seen: &[Listing]| {
        seen.iter()
            .map(|(leases, _)| ids(leases))
            .collect::<Vec<_>>()
    };
    // What the listings hold, so that equal listings are not equal for being empty.
    let expected: Vec<Vec<&str>> = vec![
        vec!["1-1", "1-2"],
        vec!["1-1", "1-2", "1-3"],
        vec!["1-1", "1-2", "1-3"],
        vec!["1-1"],
        vec!["1-4"],
    ];
    assert_eq!(leases(&looped), expected);
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

/// Asserts that a lease survives a restart through the seam: after `shutdown`, a second
/// server over the same lease file with `--boot resume` lists it. Before that, a second
/// server is refused naming `--state-file` while the first still holds the file, and again
/// while a clone of the first's router outlives the shut-down handle (the clone holds the
/// table, as the module doc says), so the start after the clone is dropped is one the lock
/// would have refused. It catches a `shutdown` that leaves the lock on the lease file held,
/// and a lease file the seam does not write.
#[tokio::test]
async fn a_second_in_process_server_starts_over_the_state_file_after_the_first_is_shut_down() {
    let scratch = Scratch::new("restart");
    let monotonic = Stepping::of(Duration::from_millis(1));
    let (_clock, shared) = clock();
    let first =
        InProcess::start(config(&scratch, "initialise", false, &monotonic), shared).unwrap();
    let client = first.client_router();
    let granted = routed(
        &client,
        "POST",
        "/v1/leases",
        Some(TOKEN),
        Some(&lease_body()),
    )
    .await;
    assert_eq!(granted.status, 201, "{:?}", granted.body);

    let resume = || {
        let (_clock, shared) = clock();
        InProcess::start(config(&scratch, "resume", false, &monotonic), shared)
    };
    let refused = resume()
        .err()
        .expect("the first server holds the lease file");
    assert!(refused.to_string().starts_with("--state-file"), "{refused}");
    first.shutdown();
    let still = resume()
        .err()
        .expect("the router's clone holds the lease file");
    assert!(still.to_string().starts_with("--state-file"), "{still}");
    drop(client);

    let second = resume().unwrap();
    let client = second.client_router();
    let listed = routed(&client, "GET", "/v1/leases", Some(TOKEN), None).await;
    assert_eq!(ids(&listed), vec!["1-1"]);
    assert_eq!(listed.body["leases"][0]["host"], granted.body["host"]);
}
