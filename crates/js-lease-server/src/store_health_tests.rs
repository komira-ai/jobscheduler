//! The store's health as the routes read it: a server with the job tables over a store that a
//! switch turns healthy or failing, its tick stepped by the test and its clock manual, asked
//! through the router as a private peer asks; and the readings, the alert's delivery and the
//! settings on their own. Nothing waits on time.

use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use js_alert::FakeSink;
use js_jobs::{JobId, JobStore, JobStoreError};
use js_lease::{Boot, LeaseTable, ManualClock, Settings};
use js_registry::{PoolConfig, Registry, SequentialSessions};
use js_state::fake::{Fake, Flaw};
use js_state::{
    Changes, Commit, CommitStats, Conflict, ManualClock as StoreClock, Revision, ScanPage,
    ScanRange, Store, StoreError, StoreTime, Txn, Versioned,
};
use js_testkit::dir::TestDir;
use js_types::{ProviderId, ResourceClass, Resources, Shape};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::*;
use crate::adapt::BoxedPool;
use crate::agent_api::job_rig::{OneClock, START_MS};
use crate::agents::Agents;
use crate::api::{Shared, router};
use crate::config::clients_rig::ClientsRig;
use crate::config::{Cli, parse_duration, parse_token};
use crate::job_store::ServerIds;
use crate::server::{Opened, open, prepare};
use crate::status::{Status, escape};

/// The window of the servers here.
const WINDOW: Duration = Duration::from_secs(30);

/// The text of the error a failing [`Switch`] answers.
const GONE: &str = "the store went away (switched off)";

/// A store that passes every call to the store it wraps while healthy and, while failing,
/// answers every call that reads or writes with [`StoreError::Disconnected`], as a Postgres
/// store whose server went away does. The call after [`Switch::slow_next`] first moves the
/// clock by the time given, as a slow round trip would.
struct Switch {
    inner: Fake,
    failing: AtomicBool,
    clock: Arc<StoreClock>,
    slow: Mutex<Option<Duration>>,
}

impl Switch {
    fn new(clock: Arc<StoreClock>) -> Switch {
        Switch {
            inner: Fake::with_flaw(Arc::clone(&clock), Flaw::None),
            failing: AtomicBool::new(false),
            clock,
            slow: Mutex::new(None),
        }
    }

    /// Turns the store failing (`true`) or healthy (`false`).
    fn fail(&self, failing: bool) {
        self.failing.store(failing, Ordering::SeqCst);
    }

    /// Makes the next call take `took` on the clock.
    fn slow_next(&self, took: Duration) {
        *self.slow.lock().unwrap_or_else(PoisonError::into_inner) = Some(took);
    }

    /// What a call meets before it reaches the store.
    fn gate(&self) -> Result<(), StoreError> {
        let slow = self
            .slow
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        slow.into_iter().for_each(|took| self.clock.advance(took));
        match self.failing.load(Ordering::SeqCst) {
            true => Err(StoreError::Disconnected(GONE.to_owned())),
            false => Ok(()),
        }
    }
}

impl Store for Switch {
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        self.gate()?;
        self.inner.apply_batch(batch)
    }

    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        self.gate()?;
        self.inner.get(key)
    }

    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        self.gate()?;
        self.inner.scan_prefix(prefix)
    }

    fn scan_range(&self, scan: ScanRange<'_>) -> Result<ScanPage, StoreError> {
        self.gate()?;
        self.inner.scan_range(scan)
    }

    fn changes(&self, after: Revision, limit: usize) -> Result<Changes, StoreError> {
        self.gate()?;
        self.inner.changes(after, limit)
    }

    fn trim_feed(&self, before: Revision) -> Result<u64, StoreError> {
        self.gate()?;
        self.inner.trim_feed(before)
    }

    fn reap_expired(&self) -> Result<usize, StoreError> {
        self.gate()?;
        self.inner.reap_expired()
    }

    fn commit_stats(&self) -> CommitStats {
        self.inner.commit_stats()
    }

    fn now(&self) -> StoreTime {
        self.inner.now()
    }
}

/// A server holding the job tables over a [`Switch`], with the status routes served, its
/// health unreachable after [`WINDOW`], and one manual clock for the server and the store.
struct Bed {
    shared: Arc<Shared>,
    health: Arc<StoreHealth>,
    store: Arc<Switch>,
    clock: Arc<StoreClock>,
    _dir: TestDir,
}

/// A [`Bed`] in a directory named `name`, its alert sent to `sink` when there is one.
fn bed(name: &str, sink: Option<Arc<FakeSink>>) -> Bed {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/store_health");
    let dir = TestDir::under(root, name);
    let clock = Arc::new(StoreClock::default());
    clock.advance(Duration::from_millis(START_MS));
    let server_clock = SharedClock::new(OneClock(Arc::clone(&clock)));
    let store = Arc::new(Switch::new(Arc::clone(&clock)));
    let dynamic: Arc<dyn Store> = store.clone();
    let jobs = JobStore::new(Arc::new(dynamic), ServerIds::random()).unwrap();
    let resources = Resources::new(8000, 17179869184, 0, None).unwrap();
    let shape = Shape::new(resources, Default::default());
    let pool = PoolConfig::new(
        ProviderId::new("burst").unwrap(),
        ResourceClass::gpu_host(),
        shape,
        4,
    );
    let registry = Registry::with_sessions(pool, server_clock.clone(), SequentialSessions::new());
    let agents = Agents::over(registry, &server_clock, dir.path()).with_jobs(Arc::new(jobs));
    let settings = Settings::new();
    let table = LeaseTable::open(
        BoxedPool::new(Box::new(agents.registry.provider())),
        server_clock.clone(),
        settings,
        dir.path().join("leases.toml"),
        Boot::Initialise,
    )
    .unwrap();
    let health = StoreHealth::new(server_clock.clone(), WINDOW);
    let health = Arc::new(match sink {
        Some(sink) => health.with_sink(sink, Duration::from_secs(3600)),
        None => health,
    });
    let status = Status::new(None, None, None, Vec::new(), server_clock)
        .with_store_health(Arc::clone(&health));
    let token = parse_token(&[b'c'; 40]).unwrap();
    let shared = Shared::new(table, token, Some(Arc::new(agents)))
        .with_status(Some(Arc::new(status)))
        .with_store_health(Arc::clone(&health));
    Bed {
        shared: Arc::new(shared),
        health,
        store,
        clock,
        _dir: dir,
    }
}

impl Bed {
    /// The status and body of `GET path`, asked through the whole router by a private peer.
    async fn get(&self, path: &str) -> (StatusCode, String) {
        let mut request = Request::get(path).body(Body::empty()).unwrap();
        let peer: SocketAddr = "10.0.0.5:4000".parse().unwrap();
        request.extensions_mut().insert(ConnectInfo(peer));
        let reply = router(self.shared.clone(), false)
            .oneshot(request)
            .await
            .unwrap();
        let status = reply.status();
        let body = to_bytes(reply.into_body(), 1 << 20).await.unwrap();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    /// The `store` object of `GET /v1/pools`.
    async fn store_view(&self) -> Value {
        let (status, body) = self.get("/v1/pools").await;
        assert_eq!(status, StatusCode::OK);
        let pools: Value = serde_json::from_str(&body).unwrap();
        pools["store"].clone()
    }

    /// The server's time now, in milliseconds.
    fn now_ms(&self) -> u64 {
        let since = js_state::Clock::now(&*self.clock).as_duration();
        u64::try_from(since.as_millis()).unwrap()
    }
}

/// `/healthz` answering 200 `ok`.
fn ok() -> (StatusCode, String) {
    (StatusCode::OK, "ok".to_owned())
}

/// The text of the `store:unreachable` alert after `failures` failed ticks whose last failed
/// with `kind`, for a window of [`WINDOW`].
fn unreachable_text(failures: u64, kind: &str) -> String {
    format!(
        "the store has failed {failures} ticks in a row for 30 s or more (last: {kind}); /healthz answers 503 until a tick's store work succeeds"
    )
}

/// Asserts the whole path an operator sees, through the routes: a healthy store is `/healthz`
/// 200 with no failure and no alert; a failing one is still 200 below the window, with the
/// count and the kind on the status route; at the window (1 ms short of it is still 200)
/// `/healthz` is 503 `store unreachable` and `store:unreachable` holds; one tick whose store
/// work succeeds makes it 200 again, clears the alert and the count, and keeps the last
/// error's kind. Catches a `/healthz` that reads only the tick loop, a status route without
/// the store, and a streak that a success does not end.
#[tokio::test]
async fn healthz_is_503_with_cause_store_from_the_window_until_one_tick_succeeds() {
    let bed = bed("route", None);
    bed.shared.tick().await;
    assert_eq!(bed.get("/healthz").await, ok());
    let start = bed.now_ms();
    assert_eq!(
        bed.store_view().await,
        json!({
            "consecutive_failures": 0,
            "failing_since_ms": null,
            "last_error": null,
            "last_ok_ms": start,
            "latency_ms": 0,
            "lock": "none",
            "unreachable_after_ms": 30_000,
            "unreachable": false,
            "alerts": {},
        })
    );

    bed.store.fail(true);
    let failed_at = bed.now_ms();
    for _ in 0..3 {
        bed.shared.tick().await;
        bed.clock.advance(Duration::from_secs(5));
    }
    assert_eq!(bed.get("/healthz").await, ok());
    let store = bed.store_view().await;
    assert_eq!(store["consecutive_failures"], 3);
    assert_eq!(store["last_error"], "disconnected");
    assert_eq!(store["failing_since_ms"], failed_at);
    assert_eq!(store["last_ok_ms"], start);
    assert_eq!(store["unreachable"], false);
    assert_eq!(store["alerts"], json!({}));

    bed.clock
        .advance(WINDOW - Duration::from_secs(15) - Duration::from_millis(1));
    assert_eq!(bed.get("/healthz").await, ok());
    bed.clock.advance(Duration::from_millis(1));
    assert_eq!(
        bed.get("/healthz").await,
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "store unreachable".to_owned()
        )
    );
    let store = bed.store_view().await;
    assert_eq!(store["unreachable"], true);
    assert_eq!(
        store["alerts"],
        json!({ "store:unreachable": unreachable_text(3, "disconnected") })
    );
    let (_, page) = bed.get("/status").await;
    assert!(page.contains("<h2>Store</h2>"), "{page}");
    let row = format!(
        "<tr><th align=\"left\">alert</th><td>{}</td></tr>",
        escape(&unreachable_text(3, "disconnected"))
    );
    assert!(page.contains(&row), "{page}");

    bed.store.fail(false);
    bed.shared.tick().await;
    assert_eq!(bed.get("/healthz").await, ok());
    let store = bed.store_view().await;
    assert_eq!(store["consecutive_failures"], 0);
    assert_eq!(store["failing_since_ms"], Value::Null);
    assert_eq!(store["last_error"], "disconnected");
    assert_eq!(store["last_ok_ms"], bed.now_ms());
    assert_eq!(store["unreachable"], false);
    assert_eq!(store["alerts"], json!({}));
}

/// Asserts that a stopped tick loop is named before an unreachable store: with both,
/// `/healthz` answers 503 `tick loop stopped`. Catches a cause that hides the stopped loop,
/// which no supervisor would then restart for.
#[tokio::test]
async fn a_stopped_tick_loop_is_named_before_the_store() {
    let bed = bed("both", None);
    bed.store.fail(true);
    bed.shared.tick().await;
    bed.clock.advance(WINDOW);
    assert!(bed.health.unreachable());
    bed.shared.stop_ticking();
    assert_eq!(
        bed.get("/healthz").await,
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "tick loop stopped".to_owned()
        )
    );
}

/// Asserts that the latency is how long the last job pass that ran took on the server's
/// clock, and that a failed tick leaves it as it was. Catches a latency read from another
/// clock, or one a failure overwrites.
#[tokio::test]
async fn the_latency_is_the_last_successful_pass_on_the_servers_clock() {
    let bed = bed("latency", None);
    bed.store.slow_next(Duration::from_millis(250));
    bed.shared.tick().await;
    assert_eq!(bed.store_view().await["latency_ms"], 250);
    bed.store.fail(true);
    bed.store.slow_next(Duration::from_millis(900));
    bed.shared.tick().await;
    let store = bed.store_view().await;
    assert_eq!(store["latency_ms"], 250);
    assert_eq!(store["consecutive_failures"], 1);
}

/// Asserts that the alert reaches the sink once when the store becomes unreachable, not
/// again while it stays so (within `repeat_after`), and again when it returns after a
/// success cleared it. Catches an alert sent at every tick, and one never sent again.
#[tokio::test]
async fn the_alert_goes_to_the_sink_once_while_it_holds_and_again_when_it_returns() {
    let sink = Arc::new(FakeSink::new());
    let bed = bed("sink", Some(Arc::clone(&sink)));
    bed.store.fail(true);
    bed.shared.tick().await;
    bed.health.flush_alerts();
    assert!(sink.keys().is_empty());
    bed.clock.advance(WINDOW);
    for _ in 0..3 {
        bed.shared.tick().await;
        bed.health.flush_alerts();
    }
    assert_eq!(sink.keys(), [UNREACHABLE]);
    assert_eq!(sink.sent()[0].text, unreachable_text(2, "disconnected"));
    bed.store.fail(false);
    bed.shared.tick().await;
    bed.store.fail(true);
    bed.shared.tick().await;
    bed.clock.advance(WINDOW);
    bed.shared.tick().await;
    bed.health.flush_alerts();
    assert_eq!(sink.keys(), [UNREACHABLE, UNREACHABLE]);
}

/// The health on `clock` with the window [`WINDOW`].
fn health_on(clock: &ManualClock) -> StoreHealth {
    StoreHealth::new(SharedClock::new(clock.clone()), WINDOW)
}

/// Asserts how one tick's two paths make one reading: the lease file alone failing is a
/// `lease_file` failure; both failing is the job pass's kind; a tick without a job pass is a
/// success that keeps the latency of the last pass that ran. Catches a lease-file failure
/// that is not counted, a kind that hides the store's, and a latency lost to a tick that ran
/// no pass.
#[test]
fn a_tick_is_one_reading_of_both_paths() {
    let clock = ManualClock::new(Timestamp::from_millis(START_MS));
    let health = health_on(&clock);
    health.record(TickStore {
        lease_file_failed: false,
        pass: Some(Pass::Ran(Duration::from_millis(40))),
    });
    health.record(TickStore {
        lease_file_failed: true,
        pass: None,
    });
    let view = health.view();
    assert_eq!(
        (view.consecutive_failures, view.last_error),
        (1, Some(FailureKind::LeaseFile))
    );
    health.record(TickStore {
        lease_file_failed: true,
        pass: Some(Pass::Failed(FailureKind::Failed)),
    });
    let view = health.view();
    assert_eq!(
        (view.consecutive_failures, view.last_error),
        (2, Some(FailureKind::Failed))
    );
    clock.set(Timestamp::from_millis(START_MS + 7));
    health.record(TickStore::default());
    let view = health.view();
    assert_eq!(view.consecutive_failures, 0);
    assert_eq!(view.last_ok_ms, Some(START_MS + 7));
    assert_eq!(view.latency_ms, Some(40));
}

/// Asserts the kind of each error of the store, and that an error that is not the store's
/// has none: the store answered. Each kind is spelled the same on the route and the page.
/// Catches a refused operation counted as an unreachable store, and two spellings of a kind.
#[test]
fn each_store_error_has_its_kind_and_others_have_none() {
    let floor = Revision(9);
    let cases = [
        (
            StoreError::Disconnected(GONE.to_owned()),
            FailureKind::Disconnected,
        ),
        (StoreError::Failed("disk".to_owned()), FailureKind::Failed),
        (StoreError::Fenced, FailureKind::Fenced),
        (
            StoreError::StaleCursor {
                after: Revision(1),
                floor,
            },
            FailureKind::StaleCursor,
        ),
    ];
    for (error, kind) in cases {
        assert_eq!(FailureKind::of(&JobStoreError::Store(error)), Some(kind));
    }
    let id = JobId::parse("job-x").unwrap();
    assert_eq!(FailureKind::of(&JobStoreError::NotFound { id }), None);
    for (kind, name) in [
        (FailureKind::Disconnected, "disconnected"),
        (FailureKind::Failed, "failed"),
        (FailureKind::Fenced, "fenced"),
        (FailureKind::StaleCursor, "stale_cursor"),
        (FailureKind::LeaseFile, "lease_file"),
    ] {
        assert_eq!(kind.name(), name);
        assert_eq!(serde_json::to_value(kind).unwrap(), name);
    }
}

/// Asserts the page's rows of the store for each lock state, before any reading and after a
/// failure: every field has its row, a missing time reads `none`, and the lock is spelled as
/// on the route. Catches a row left off the page and a lock state shown under another name.
#[test]
fn the_page_rows_show_every_field_and_each_lock_state() {
    let clock = ManualClock::new(Timestamp::from_millis(START_MS));
    let health = health_on(&clock);
    for (lock, name) in [
        (LockState::None, "none"),
        (LockState::Held, "held"),
        (LockState::Lost, "lost"),
    ] {
        health.set_lock(lock);
        let view = health.view();
        assert_eq!(serde_json::to_value(view.lock).unwrap(), name);
        let rows = rows(&view);
        assert_eq!(rows[5], ("lock", name.to_owned()));
    }
    let row = |label: &'static str, value: &str| (label, value.to_owned());
    assert_eq!(
        rows(&health.view()),
        [
            row("consecutive failures", "0"),
            row("failing since", "none"),
            row("last error", "none"),
            row("last ok", "none"),
            row("latency", "none"),
            row("lock", "lost"),
            row("unreachable", "false"),
        ]
    );
    health.record(TickStore {
        lease_file_failed: false,
        pass: Some(Pass::Failed(FailureKind::Fenced)),
    });
    let rows = rows(&health.view());
    assert_eq!(rows[1], row("failing since", &format!("{START_MS} ms")));
    assert_eq!(rows[2], row("last error", "fenced"));
}

/// Asserts the window's settings: 60 s unless given, the flag's default spelling and the
/// constant agreeing; the flag and the file key each set it; and a zero is refused, as every
/// duration of the file is. Catches a flag that does not reach the server's health, a file key
/// that is ignored, and a default that differs between the flag and the file.
#[test]
fn the_window_is_sixty_seconds_unless_the_flag_or_the_file_says_otherwise() {
    assert_eq!(
        parse_duration(DEFAULT_UNREACHABLE_AFTER),
        Ok(UNREACHABLE_AFTER)
    );
    assert_eq!(UNREACHABLE_AFTER, Duration::from_secs(60));
    let cli = Cli::try_parse_from(["js-lease-server", "--config", "js.toml"]).unwrap();
    assert_eq!(cli.store_unreachable_after, UNREACHABLE_AFTER);
    let rig = ClientsRig::new("store_window");
    let flag: Vec<OsString> = vec!["--store-unreachable-after".into(), "90s".into()];
    let config = rig.config_of_flags(&flag).unwrap();
    assert_eq!(config.store_unreachable_after, Duration::from_secs(90));
    assert_eq!(
        rig.config_of_flags(&[]).unwrap().store_unreachable_after,
        UNREACHABLE_AFTER
    );
    let file = rig.file("store_unreachable_after = \"2m\"", "");
    let config = rig.config_of_file(&file).unwrap();
    assert_eq!(config.store_unreachable_after, Duration::from_secs(120));
    let config = rig.config_of_file(&rig.file("", "")).unwrap();
    assert_eq!(config.store_unreachable_after, UNREACHABLE_AFTER);
    let zero = rig.file("store_unreachable_after = \"0s\"", "");
    let refused = rig.config_of_file(&zero).err().unwrap();
    assert_eq!(refused.flag(), "--config");
    assert!(
        refused
            .to_string()
            .contains("server.store_unreachable_after"),
        "{refused}"
    );
    assert_eq!(
        refused.reason(),
        "must be a positive duration such as 600s, 10m or 1h"
    );
}

/// Asserts that a [`Switch`] passes every call to its store while healthy and refuses every
/// call that reads or writes while failing, so the tests above fail the store and nothing else.
#[test]
fn the_switch_passes_every_call_while_healthy_and_refuses_them_while_failing() {
    let store = Switch::new(Arc::new(StoreClock::default()));
    let scan = || ScanRange::new("a/", std::num::NonZeroUsize::new(1).unwrap());
    assert_eq!(store.apply_batch(Vec::new()), Ok(Vec::new()));
    assert_eq!(store.get("a/x"), Ok(None));
    assert_eq!(store.scan_prefix("a/"), Ok(Vec::new()));
    assert!(store.scan_range(scan()).unwrap().rows.is_empty());
    assert!(store.changes(Revision(0), 1).is_ok());
    assert_eq!(store.trim_feed(Revision(0)), Ok(0));
    assert_eq!(store.reap_expired(), Ok(0));
    assert_eq!(store.commit_stats(), store.inner.commit_stats());
    assert_eq!(store.now(), store.inner.now());
    store.fail(true);
    let gone = StoreError::Disconnected(GONE.to_owned());
    assert_eq!(store.apply_batch(Vec::new()), Err(gone.clone()));
    assert_eq!(store.get("a/x"), Err(gone.clone()));
    assert_eq!(store.scan_prefix("a/"), Err(gone.clone()));
    assert_eq!(store.scan_range(scan()).err(), Some(gone.clone()));
    assert_eq!(store.changes(Revision(0), 1).err(), Some(gone.clone()));
    assert_eq!(store.trim_feed(Revision(0)), Err(gone.clone()));
    assert_eq!(store.reap_expired(), Err(gone));
}

/// Asserts that the tick reads the lease file's write: with the file behind the table and
/// its write blocked, a tick whose job pass succeeds is still one `lease_file` failure on the
/// status route, and the tick after the write is unblocked succeeds. Catches a lease-file
/// failure left out of the reading, and one that the job pass's success in the same tick
/// resets.
#[tokio::test]
async fn a_failed_lease_file_write_is_a_failure_even_when_the_job_pass_succeeds() {
    let bed = bed("lease_file", None);
    bed.shared.tick().await;
    let before = bed.store_view().await["last_ok_ms"].clone();
    let blocker = bed._dir.path().join("leases.toml.tmp");
    std::fs::create_dir(&blocker).unwrap();
    bed.shared.table().lock().await.set_reservations(Vec::new());
    bed.clock.advance(Duration::from_secs(5));
    bed.shared.tick().await;
    assert_eq!(bed.shared.save_errors(), 1);
    let store = bed.store_view().await;
    assert_eq!(store["consecutive_failures"], 1);
    assert_eq!(store["last_error"], "lease_file");
    assert_eq!(store["last_ok_ms"], before);
    std::fs::remove_dir(&blocker).unwrap();
    bed.shared.tick().await;
    let store = bed.store_view().await;
    assert_eq!(store["consecutive_failures"], 0);
    assert_eq!(store["last_ok_ms"], bed.now_ms());
}

/// Asserts that a server built from its configuration, with no elastic pool and so no
/// scaler, gives its store health the window of `--store-unreachable-after` and the alert
/// sink of the configuration, which receives `store:unreachable`; and that without a store
/// setting the lock is `none`. Catches a sink handed only to a scaler, and a flag that does
/// not reach the server.
#[tokio::test]
async fn a_server_without_a_scaler_sends_the_store_alert_to_its_sink() {
    let rig = ClientsRig::new("store_sink");
    let flag: Vec<OsString> = vec!["--store-unreachable-after".into(), "1s".into()];
    let sink = Arc::new(FakeSink::new());
    let config = rig
        .config_of_flags(&flag)
        .unwrap()
        .with_alert_sink(sink.clone());
    let clock = ManualClock::new(Timestamp::from_millis(START_MS));
    let server_clock = SharedClock::new(clock.clone());
    let (prepared, _) = prepare(config, &server_clock).unwrap();
    let Opened { shared, scaler, .. } = open(prepared, &server_clock, |_| {}).unwrap();
    assert!(scaler.is_none());
    let health = Arc::clone(shared.store_health().unwrap());
    let view = health.view();
    assert_eq!(
        (view.lock, view.unreachable_after_ms),
        (LockState::None, 1000)
    );
    let failed = TickStore {
        lease_file_failed: true,
        pass: None,
    };
    health.record(failed);
    clock.set(Timestamp::from_millis(START_MS + 1000));
    health.record(failed);
    health.flush_alerts();
    assert_eq!(sink.keys(), [UNREACHABLE]);
}
