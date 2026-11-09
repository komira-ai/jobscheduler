//! The tick loop: the only place time acts, driven by a ticker the test steps and a
//! clock the test moves.

use js_lease_server::{SharedClock, Tick, Ticker, start};

use crate::support::{Harness, Scratch, TOKEN, base_args, request};

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
/// ticks should be taken out of rotation, not just look alive.
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
    let clock = SharedClock::new(js_types::ManualClock::default());
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
