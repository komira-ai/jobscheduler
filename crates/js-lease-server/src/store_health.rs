//! How the server's store paths fared, tick by tick: what `/healthz`, the status routes and
//! the `store:unreachable` alert read.
//!
//! Each tick is one reading ([`TickStore`]) of the two store paths a tick takes: the write
//! of the lease file, when the tick changed something to write, and the job pass over the job
//! tables, when the server holds them ([`crate::agents::Agents::job_pass`]). A tick in which
//! either failed is a failure of the kind of the one that failed (the job pass's when both
//! did); any other tick is a success. A job pass that failed for a reason that is not the
//! store's (a job the pass expected and did not find, say) is no reading of the store: the
//! store answered.
//!
//! - A failure adds one to the consecutive failures, names its [`FailureKind`], and starts
//!   the streak's clock at its tick when it is the first.
//! - A success sets the consecutive failures to 0, ends the streak, and stamps the tick as
//!   the last good one, with the job pass's latency when the tick ran one.
//!
//! The store is **unreachable** once a streak has lasted at least the window
//! (`--store-unreachable-after`, `server.store_unreachable_after`, 60 s unless given), as
//! the server's clock reads it, at the moment of the read: `/healthz` answers 503 with the
//! cause `store`, and the alert `store:unreachable` holds. The first success clears both.
//! The alert is logged once when a tick finds it holding, and goes to the alert sink of
//! `server.alert_webhook_url` when there is one, through the same dedupe as the scaler's
//! alerts ([`js_alert::Notifier`]).
//!
//! Time is the server's injected clock and nothing else: no reading here takes the wall
//! clock.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use js_alert::{AlertSink, Notifier};
use js_jobs::JobStoreError;
use js_state::StoreError;
use js_types::{Clock, Timestamp};
use serde::Serialize;
use tracing::{info, warn};

use crate::adapt::SharedClock;

/// The key of the alert that holds while the store is unreachable.
pub(crate) const UNREACHABLE: &str = "store:unreachable";

/// The window unless `--store-unreachable-after` or `server.store_unreachable_after` gives
/// another.
pub(crate) const UNREACHABLE_AFTER: Duration = Duration::from_secs(60);

/// [`UNREACHABLE_AFTER`] as the flag's default spells it.
pub(crate) const DEFAULT_UNREACHABLE_AFTER: &str = "60s";

/// The most alerts waiting for the sink, as the scaler's queue.
const ALERT_QUEUE: usize = 64;

/// Why a tick's store work failed: a closed set, never the error's text, which may name the
/// store's host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FailureKind {
    /// The connection to the store was gone ([`StoreError::Disconnected`]).
    Disconnected,
    /// The store could not complete the call ([`StoreError::Failed`]).
    Failed,
    /// The store has a newer writer ([`StoreError::Fenced`]); the fence ends the server.
    Fenced,
    /// A read of the store's change feed began past its trim ([`StoreError::StaleCursor`]).
    StaleCursor,
    /// The lease file could not be written.
    LeaseFile,
}

impl FailureKind {
    /// The kind of `error` when the store failed; `None` when the store answered and the
    /// operation was refused for another reason.
    pub(crate) fn of(error: &JobStoreError) -> Option<FailureKind> {
        match error {
            JobStoreError::Store(StoreError::Disconnected(_)) => Some(FailureKind::Disconnected),
            JobStoreError::Store(StoreError::Failed(_)) => Some(FailureKind::Failed),
            JobStoreError::Store(StoreError::Fenced) => Some(FailureKind::Fenced),
            JobStoreError::Store(StoreError::StaleCursor { .. }) => Some(FailureKind::StaleCursor),
            _ => None,
        }
    }

    /// The kind as the status routes spell it.
    pub(crate) fn name(self) -> &'static str {
        match self {
            FailureKind::Disconnected => "disconnected",
            FailureKind::Failed => "failed",
            FailureKind::Fenced => "fenced",
            FailureKind::StaleCursor => "stale_cursor",
            FailureKind::LeaseFile => "lease_file",
        }
    }
}

/// How the job pass of a tick went, as far as the store is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pass {
    /// It ran to its end, in this long on the server's clock.
    Ran(Duration),
    /// The store failed it.
    Failed(FailureKind),
}

/// What one tick's store paths did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct TickStore {
    /// The tick's write of the lease file failed.
    pub(crate) lease_file_failed: bool,
    /// The job pass, when the server holds the job tables and the pass read the store.
    pub(crate) pass: Option<Pass>,
}

impl TickStore {
    /// The kind of the tick's failure: the job pass's, else the lease file's; `None` when
    /// nothing failed.
    fn failure(self) -> Option<FailureKind> {
        let pass = match self.pass {
            Some(Pass::Failed(kind)) => Some(kind),
            _ => None,
        };
        pass.or(self.lease_file_failed.then_some(FailureKind::LeaseFile))
    }
}

/// The store's writer lock (`--store`), as the server last saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LockState {
    /// The server was given no store setting, so it holds no lock.
    #[default]
    None,
    /// The lock is held.
    Held,
    /// A tick found the lock lost; the server is stopping.
    Lost,
}

impl LockState {
    fn name(self) -> &'static str {
        match self {
            LockState::None => "none",
            LockState::Held => "held",
            LockState::Lost => "lost",
        }
    }
}

/// What the readings so far say.
#[derive(Debug, Clone, Copy, Default)]
struct Seen {
    failures: u64,
    failing_since: Option<Timestamp>,
    last_error: Option<FailureKind>,
    last_ok: Option<Timestamp>,
    latency: Option<Duration>,
    lock: LockState,
    /// Whether the last reading found the alert holding, so it is logged once.
    raised: bool,
}

/// The store's health, shared by the tick that records it and the routes that read it.
pub(crate) struct StoreHealth {
    clock: SharedClock,
    window: Duration,
    seen: Mutex<Seen>,
    sink: Option<Mutex<Notifier>>,
}

/// The `store` object of the status routes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct StoreView {
    /// Ticks in a row whose store work failed; 0 after a tick whose work all succeeded.
    pub(crate) consecutive_failures: u64,
    /// The server's time at the first failed tick of the streak; null outside a streak.
    pub(crate) failing_since_ms: Option<u64>,
    /// Why the last failed tick failed; null before the first. Kept after a success.
    pub(crate) last_error: Option<FailureKind>,
    /// The server's time at the last tick whose store work all succeeded; null before one.
    pub(crate) last_ok_ms: Option<u64>,
    /// How long the last job pass that ran to its end took, in milliseconds of the server's
    /// clock; null before one, and always without the job tables.
    pub(crate) latency_ms: Option<u64>,
    /// The store's writer lock: `none` without a store setting, `held`, or `lost`.
    pub(crate) lock: LockState,
    /// How long a streak lasts before the store is unreachable, in milliseconds.
    pub(crate) unreachable_after_ms: u64,
    /// Whether the store is unreachable now: `/healthz` answers 503.
    pub(crate) unreachable: bool,
    /// The alerts that hold now, by key: `store:unreachable` while the store is unreachable.
    pub(crate) alerts: BTreeMap<String, String>,
}

impl StoreHealth {
    /// No reading yet, on `clock`, unreachable after a streak of `window`.
    pub(crate) fn new(clock: SharedClock, window: Duration) -> StoreHealth {
        StoreHealth {
            clock,
            window,
            seen: Mutex::new(Seen::default()),
            sink: None,
        }
    }

    /// The same health, sending its alert to `sink` as well as the log, again every
    /// `repeat_after` while it holds.
    #[must_use]
    pub(crate) fn with_sink(self, sink: Arc<dyn AlertSink>, repeat_after: Duration) -> Self {
        let notifier = Notifier::new(sink, repeat_after, ALERT_QUEUE);
        StoreHealth {
            sink: Some(Mutex::new(notifier)),
            ..self
        }
    }

    fn seen(&self) -> MutexGuard<'_, Seen> {
        self.seen.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records the writer lock's state.
    pub(crate) fn set_lock(&self, lock: LockState) {
        self.seen().lock = lock;
    }

    /// Records one tick's store work at the server's time now, logs the alert when it
    /// begins or stops holding, and hands the alerts that hold to the sink.
    pub(crate) fn record(&self, tick: TickStore) {
        let now = self.clock.now();
        let mut seen = self.seen();
        match tick.failure() {
            Some(kind) => {
                seen.failures = seen.failures.saturating_add(1);
                seen.failing_since.get_or_insert(now);
                seen.last_error = Some(kind);
            }
            None => {
                seen.failures = 0;
                seen.failing_since = None;
                seen.last_ok = Some(now);
                if let Some(Pass::Ran(took)) = tick.pass {
                    seen.latency = Some(took);
                }
            }
        }
        let found = self.alerts(&seen, now);
        let raised = !found.is_empty();
        match (seen.raised, raised) {
            (false, true) => found.values().for_each(|text| warn!("{text}")),
            (true, false) => info!("the store answers again: {UNREACHABLE} cleared"),
            _ => {}
        }
        seen.raised = raised;
        drop(seen);
        if let Some(notifier) = &self.sink {
            let at = Duration::from_millis(now.millis());
            let mut notifier = notifier.lock().unwrap_or_else(PoisonError::into_inner);
            notifier.pass(at, &found);
        }
    }

    /// Whether `seen` has a streak that has lasted the window at `now`.
    fn past_window(&self, seen: &Seen, now: Timestamp) -> bool {
        seen.failing_since.is_some_and(|since| {
            Duration::from_millis(now.millis().saturating_sub(since.millis())) >= self.window
        })
    }

    /// The alerts that hold for `seen` at `now`.
    fn alerts(&self, seen: &Seen, now: Timestamp) -> BTreeMap<String, String> {
        let kind = seen.last_error.map_or("unknown", FailureKind::name);
        let text = format!(
            "the store has failed {} ticks in a row for {} s or more (last: {kind}); /healthz answers 503 until a tick's store work succeeds",
            seen.failures,
            self.window.as_secs(),
        );
        let holds = self.past_window(seen, now);
        holds
            .then(|| (UNREACHABLE.to_owned(), text))
            .into_iter()
            .collect()
    }

    /// Whether the store is unreachable now: a streak of failures has lasted the window.
    pub(crate) fn unreachable(&self) -> bool {
        let now = self.clock.now();
        self.past_window(&self.seen(), now)
    }

    /// The store's health now, for the status routes.
    pub(crate) fn view(&self) -> StoreView {
        let now = self.clock.now();
        let seen = *self.seen();
        StoreView {
            consecutive_failures: seen.failures,
            failing_since_ms: seen.failing_since.map(|at| at.millis()),
            last_error: seen.last_error,
            last_ok_ms: seen.last_ok.map(|at| at.millis()),
            latency_ms: seen
                .latency
                .map(|took| u64::try_from(took.as_millis()).unwrap_or(u64::MAX)),
            lock: seen.lock,
            unreachable_after_ms: u64::try_from(self.window.as_millis()).unwrap_or(u64::MAX),
            unreachable: self.past_window(&seen, now),
            alerts: self.alerts(&seen, now),
        }
    }

    /// Waits until every alert handed to the sink has been answered. For tests.
    #[cfg(test)]
    pub(crate) fn flush_alerts(&self) {
        let flush = |notifier: &Mutex<Notifier>| {
            let notifier = notifier.lock().unwrap_or_else(PoisonError::into_inner);
            notifier.flush();
        };
        self.sink.iter().for_each(flush);
    }
}

/// The `store` object as rows of the status page.
pub(crate) fn rows(view: &StoreView) -> Vec<(&'static str, String)> {
    let ms = |at: Option<u64>| at.map_or_else(|| "none".to_owned(), |at| format!("{at} ms"));
    let mut rows = vec![
        (
            "consecutive failures",
            view.consecutive_failures.to_string(),
        ),
        ("failing since", ms(view.failing_since_ms)),
        (
            "last error",
            view.last_error.map_or("none", FailureKind::name).to_owned(),
        ),
        ("last ok", ms(view.last_ok_ms)),
        ("latency", ms(view.latency_ms)),
        ("lock", view.lock.name().to_owned()),
        ("unreachable", view.unreachable.to_string()),
    ];
    rows.extend(view.alerts.values().map(|text| ("alert", text.clone())));
    rows
}

#[cfg(test)]
#[path = "store_health_tests.rs"]
mod tests;
