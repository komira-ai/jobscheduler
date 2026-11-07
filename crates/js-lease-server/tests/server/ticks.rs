//! The tick loop: the only place time acts, driven by a ticker the test steps and a
//! clock the test moves. The leases in these tests come from a lease file the server
//! resumes, since the table holds nothing else.

use std::time::Duration;

use js_lease_server::{SharedClock, Tick, Ticker, start};

use crate::agents_support::{entry, hosts};
use crate::support::{Harness, Scratch, TOKEN, base_args, lease_file, replace, request};

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// A server resumed over a lease file that holds one lease on `gpu-01`, acquired at the start of the test clock and
/// ending 7200 seconds later.
async fn holding(name: &str) -> Harness {
    let scratch = Scratch::new(name);
    std::fs::write(scratch.state(), lease_file(&[(1, "held", Some("gpu-01"))])).unwrap();
    let args = replace(base_args(&scratch), "--boot", "resume");
    Harness::with(scratch, args, None).await
}

/// The lease on `gpu-01` in the host list, if any.
async fn lease_on_gpu_01(h: &Harness) -> Option<String> {
    let lease = entry(&hosts(h).await, "gpu-01")["lease"].clone();
    lease.as_str().map(str::to_owned)
}

/// Asserts that a restored lease past its wall end stays listed until a tick, that the tick
/// frees it, and that its host is then free. It catches a lease freed before its wall end,
/// one freed without a tick, and one that outlives its wall end.
#[tokio::test]
async fn a_tick_ends_a_lease_at_its_wall_end() {
    let h = holding("expire").await;
    assert_eq!(lease_on_gpu_01(&h).await.as_deref(), Some("1-1"));

    h.clock.advance(secs(7199));
    h.ticks.step().await;
    assert_eq!(lease_on_gpu_01(&h).await.as_deref(), Some("1-1"));

    h.clock.advance(secs(2));
    assert_eq!(lease_on_gpu_01(&h).await.as_deref(), Some("1-1"));

    h.ticks.step().await;
    assert_eq!(lease_on_gpu_01(&h).await, None);
}

/// Asserts that a tick that cannot write the lease file is counted, that the next tick
/// writes it and is not counted, and that a tick with nothing to do counts nothing.
#[tokio::test]
async fn save_errors_are_counted() {
    let h = holding("save_errors").await;
    h.ticks.step().await;
    assert_eq!(h.running.save_errors(), 0);

    let blocker = h.scratch.path("leases.toml.tmp");
    std::fs::create_dir(&blocker).unwrap();
    h.clock.advance(secs(7201));
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

/// Asserts that shutdown ticks the table once more: a lease that reached its end since the
/// last tick is stopped and gone from the file when the server has stopped.
#[tokio::test]
async fn shutdown_ticks_once_more() {
    let h = holding("shutdown_tick").await;
    h.clock.advance(secs(7201));
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
    let listed = request(running.addr(), "GET", "/v1/hosts", Some(TOKEN), None).await;
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
    let h = holding("final_save").await;
    std::fs::create_dir(h.scratch.path("leases.toml.tmp")).unwrap();
    h.clock.advance(secs(7201));

    let named = format!(
        "the final tick could not write the lease file {}: ",
        h.scratch.state().display()
    );
    let err = h.running.shutdown().await.unwrap_err();
    let message = err.to_string();
    assert!(message.starts_with(&named), "{message}");
}
