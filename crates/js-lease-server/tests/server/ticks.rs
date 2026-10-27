//! The tick loop: the only place time acts, driven by a ticker the test steps and a
//! clock the test moves.

use std::collections::BTreeMap;
use std::time::Duration;

use js_lease::Booking;
use js_lease_server::{CapacitySource, Seated, SharedClock, StartupError, Tick, Ticker, start};
use js_provider::Call;
use js_provider_fake::FakeProvider;
use js_types::{HostId, ProviderId, Resources, Shape};
use serde_json::json;

use crate::support::{
    Gate, Harness, Scratch, SharedPool, T0, TOKEN, args_with, base_args, ids, lease_body, request,
};

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// A source of two fake hosts whose first calls are scripted to get no answer.
struct Scripted(Vec<(Call, bool)>);

impl CapacitySource for Scripted {
    fn provider(&self, _bookings: &[Booking]) -> Result<Seated, StartupError> {
        let resources = Resources::new(64_000, 1 << 30, 8, Some("nvidia-a100".into())).unwrap();
        let caps = BTreeMap::from([("isa".to_string(), "x86-64-v3".to_string())]);
        let shape = Shape::new(resources, caps);
        let hosts = ["h1", "h2"].map(|name| (HostId::new(name).unwrap(), shape.clone()));
        let mut fake = FakeProvider::new(ProviderId::new("fake").unwrap(), hosts.to_vec());
        for (call, landed) in &self.0 {
            fake.script_unknown(*call, *landed);
        }
        Ok(Seated::all(Box::new(fake)))
    }
}

async fn scripted(name: &str, script: Vec<(Call, bool)>) -> Harness {
    let scratch = Scratch::new(name);
    let args = crate::support::base_args(&scratch);
    Harness::with(scratch, args, Some(Box::new(Scripted(script)))).await
}

/// Asserts that a lease past its ttl stays listed (and cannot be renewed) until a tick,
/// that the tick frees it, and that its host is free for the next lease.
#[tokio::test]
async fn a_tick_expires_a_lease_after_its_ttl() {
    let scratch = Scratch::new("expire");
    let args = args_with(&scratch, &["--default-ttl", "60s"]);
    let h = Harness::with(scratch, args, None).await;
    let granted = h.call("POST", "/v1/leases", Some(&lease_body())).await;
    assert_eq!(granted.json()["expires_at_ms"], T0 + 60_000);

    h.clock.advance(secs(59));
    h.ticks.step().await;
    assert_eq!(ids(&h.call("GET", "/v1/leases", None).await), vec!["1-1"]);

    h.clock.advance(secs(2));
    assert_eq!(ids(&h.call("GET", "/v1/leases", None).await), vec!["1-1"]);
    let late = h.call("POST", "/v1/leases/1-1/renew", None).await;
    assert_eq!(late.json(), json!({ "error": "expired" }));

    h.ticks.step().await;
    assert!(ids(&h.call("GET", "/v1/leases", None).await).is_empty());
    let next = h.call("POST", "/v1/leases", Some(&lease_body())).await;
    assert_eq!(next.json()["host"], "gpu-01");
}

/// Asserts that a start the provider did not answer is a 202 with the lease pending,
/// that it cannot be renewed meanwhile, and that a tick resolves it to held.
#[tokio::test]
async fn a_pending_start_resolves_on_a_tick() {
    let h = scripted("pending", vec![(Call::Start, true)]).await;
    let reply = h.call("POST", "/v1/leases", Some(&lease_body())).await;
    assert_eq!(reply.status, 202, "{}", reply.body);
    assert_eq!(reply.json(), json!({ "id": "1-1", "state": "pending" }));

    let listed = h.call("GET", "/v1/leases", None).await.json();
    assert_eq!(listed["leases"][0]["state"], "pending");
    assert!(listed["leases"][0].get("host").is_none());
    let renew = h.call("POST", "/v1/leases/1-1/renew", None).await;
    assert_eq!(renew.status, 409);
    assert_eq!(renew.json(), json!({ "error": "not-held" }));

    h.ticks.step().await;
    let listed = h.call("GET", "/v1/leases", None).await.json();
    assert_eq!(listed["leases"][0]["state"], "held");
    assert_eq!(listed["leases"][0]["host"], "h1");
}

/// Asserts that a start that did not land and cannot be observed is dropped by a tick
/// once the provider says it does not exist.
#[tokio::test]
async fn a_pending_start_that_never_landed_is_dropped() {
    let h = scripted("dropped", vec![(Call::Start, false)]).await;
    let reply = h.call("POST", "/v1/leases", Some(&lease_body())).await;
    assert_eq!(reply.status, 202);
    h.ticks.step().await;
    assert!(ids(&h.call("GET", "/v1/leases", None).await).is_empty());
}

/// Asserts that a stop the provider did not answer is a 202 with the lease releasing,
/// and that a tick retries the stop and frees it.
#[tokio::test]
async fn an_unconfirmed_release_is_retried_by_a_tick() {
    let h = scripted("releasing", vec![(Call::Stop, false)]).await;
    h.call("POST", "/v1/leases", Some(&lease_body())).await;
    let reply = h.call("DELETE", "/v1/leases/1-1", None).await;
    assert_eq!(reply.status, 202);
    assert_eq!(reply.json(), json!({ "state": "releasing" }));
    let listed = h.call("GET", "/v1/leases", None).await.json();
    assert_eq!(listed["leases"][0]["state"], "releasing");

    h.ticks.step().await;
    assert!(ids(&h.call("GET", "/v1/leases", None).await).is_empty());
}

/// Asserts that a tick that cannot write the lease file is counted, that the next tick
/// writes it and is not counted, and that a tick with nothing to do counts nothing.
#[tokio::test]
async fn save_errors_are_counted() {
    let h = Harness::new("save_errors").await;
    h.call("POST", "/v1/leases", Some(&lease_body())).await;
    h.ticks.step().await;
    assert_eq!(h.running.save_errors(), 0);

    let blocker = h.scratch.path("leases.toml.tmp");
    std::fs::create_dir(&blocker).unwrap();
    h.clock.advance(secs(601));
    h.ticks.step().await;
    assert_eq!(h.running.save_errors(), 1);
    assert!(
        std::fs::read_to_string(h.scratch.state())
            .unwrap()
            .contains("lease_count = 1")
    );

    std::fs::remove_dir(&blocker).unwrap();
    h.ticks.step().await;
    assert_eq!(h.running.save_errors(), 1);
    assert!(
        std::fs::read_to_string(h.scratch.state())
            .unwrap()
            .contains("lease_count = 0")
    );
}

/// Asserts that shutdown ticks the table once more: a lease that fell due since the
/// last tick is stopped and gone from the file when the server has stopped.
#[tokio::test]
async fn shutdown_ticks_once_more() {
    let h = Harness::new("shutdown_tick").await;
    h.call("POST", "/v1/leases", Some(&lease_body())).await;
    h.clock.advance(secs(601));
    let state = h.scratch.state();
    assert!(
        std::fs::read_to_string(&state)
            .unwrap()
            .contains("lease_count = 1")
    );

    h.running.shutdown().await.unwrap();
    assert!(
        std::fs::read_to_string(&state)
            .unwrap()
            .contains("lease_count = 0")
    );
}

/// Asserts that shutdown lets a request in flight finish before the server stops: the
/// request is begun, shutdown is started, and the request is then completed and answered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_finishes_requests_in_flight() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (entered, inside) = std::sync::mpsc::channel();
    let (release, go) = std::sync::mpsc::channel();
    let pool = SharedPool::new().gated(Gate { entered, go });
    let scratch = Scratch::new("shutdown_inflight");
    let args = crate::support::base_args(&scratch);
    let h = Harness::with(scratch, args, Some(Box::new(pool))).await;
    let state = h.scratch.state();
    let addr = h.addr();
    let mut stream = tokio::net::TcpStream::connect(h.addr()).await.unwrap();
    let body = lease_body().to_string();
    let head = format!(
        "POST /v1/leases HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: Bearer {}\r\nContent-Length: {}\r\n\r\n",
        crate::support::TOKEN,
        body.len()
    );
    stream.write_all(head.as_bytes()).await.unwrap();
    stream.write_all(body.as_bytes()).await.unwrap();
    // The provider is inside the start: the request is in flight.
    inside.recv().unwrap();

    let stopping = tokio::spawn(h.running.shutdown());
    // The listener closes when the server begins to stop: connections are refused
    // from then on, and the request in flight is still waiting on the provider.
    while tokio::net::TcpStream::connect(addr).await.is_ok() {
        tokio::task::yield_now().await;
    }
    assert!(!stopping.is_finished());

    release.send(()).unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    assert_eq!(crate::support::parse_reply(&raw).status, 201);
    stopping.await.unwrap().unwrap();
    assert!(
        std::fs::read_to_string(state)
            .unwrap()
            .contains("lease_count = 1")
    );
}

/// A ticker that ticks once and then panics, and reports when it is dropped, which
/// happens as the tick task unwinds.
struct Dying {
    ticked: bool,
    gone: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Ticker for Dying {
    async fn next(&mut self) -> Tick {
        assert!(!self.ticked, "the ticker has failed");
        self.ticked = true;
        Tick::default()
    }
}

impl Drop for Dying {
    fn drop(&mut self) {
        self.gone.take().into_iter().for_each(|gone| {
            let _ = gone.send(());
        });
    }
}

/// Asserts that `/healthz` answers 503 once the tick loop has stopped (here its ticker
/// panicked), while the routes that need no tick still answer: a server that no longer
/// expires leases should be taken out of rotation, not just look alive.
#[tokio::test]
async fn health_fails_when_the_tick_loop_has_stopped() {
    let scratch = Scratch::new("health_dead_loop");
    let config = crate::support::config(base_args(&scratch)).unwrap();
    let (gone, stopped) = tokio::sync::oneshot::channel();
    let ticker = Dying {
        ticked: false,
        gone: Some(gone),
    };
    crate::support::quiet_logs();
    let clock = SharedClock::new(js_lease::ManualClock::default());
    let running = start(config, clock, ticker).await.unwrap();
    stopped.await.unwrap();

    let health = request(running.addr(), "GET", "/healthz", None, None).await;
    assert_eq!(health.status, 503, "{}", health.body);
    let listed = request(running.addr(), "GET", "/v1/leases", Some(TOKEN), None).await;
    assert_eq!(listed.status, 200);
}

/// Asserts that `/healthz` answers 200 `ok` while the tick loop runs.
#[tokio::test]
async fn health_is_ok_while_the_tick_loop_runs() {
    let h = Harness::new("health_live_loop").await;
    h.ticks.step().await;
    let health = request(h.addr(), "GET", "/healthz", None, None).await;
    assert_eq!((health.status, health.body.as_str()), (200, "ok"));
}

/// Asserts that a shutdown whose last tick cannot write the lease file is an error that
/// says so and names the file, not a clean stop: the file is behind the table.
#[tokio::test]
async fn a_final_save_that_fails_is_an_error() {
    let h = Harness::new("final_save").await;
    h.call("POST", "/v1/leases", Some(&lease_body())).await;
    std::fs::create_dir(h.scratch.path("leases.toml.tmp")).unwrap();
    h.clock.advance(secs(601));

    let named = format!(
        "the final tick could not write the lease file {}: ",
        h.scratch.state().display()
    );
    let err = h.running.shutdown().await.unwrap_err();
    let message = err.to_string();
    assert!(message.starts_with(&named), "{message}");
}
