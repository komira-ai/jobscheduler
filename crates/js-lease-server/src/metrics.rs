//! The operator's metrics: `GET /metrics` in the Prometheus text format (version 0.0.4), on
//! a listener of its own (`--metrics-listen`, `server.metrics_listen`), off unless given.
//!
//! Two kinds of figure are served, and nothing else:
//!
//! - **Events**, counted as they happen: each job placed (and how long it waited in the
//!   queue, from its submit to its placement), each start failure by its code, each job lost,
//!   each pass over the job tables the server makes itself that failed, so its write did not
//!   commit (the job pass, a placement, a heartbeat's job listing), how long each job pass
//!   took, where each pass over the queued jobs took the job tables from (read from the store,
//!   kept from the last read, or kept with the jobs this server's submits recorded since added
//!   to them), and each launch the scaler ran (whatever its answer, a refusal made before the
//!   provider is called included) and each that failed over (a `capacity` or `quota` refusal,
//!   after which the next pass tries another offer), and each refusal of a host to a queued
//!   job by the placement, by host and by the predicate that refused (`cpu`,
//!   `predicted-memory`, `booked-memory`, `cold-cap`, `cpu-curve`), and each job whose wait for
//!   its CPU curve ran out so that it was tested flat, and each job start a host's agent refused,
//!   by host and refusal code, each try of a job's completion callback whose answer was
//!   recorded, by its result, and each attempt of a job that ended in a way that retries and
//!   was followed by another (`js_job_retries_total`, by `outcome`: `start-failed` or
//!   `host-lost`), and each job that ended so with no attempt left under its retry policy
//!   (`js_job_attempts_exhausted_total`) (`docs/design/job-retry-and-completion-callback.md`,
//!   section 4), and how long each phase of each `SubmitJob` call took
//!   (`js_submit_phase_seconds`, by `phase`, [`SubmitPhase`]), and each hello or heartbeat
//!   refused 413 `body-too-large` for a body over the cap (`js_agent_bodies_too_large_total`,
//!   by `route`, [`AgentRoute`]). A start failure or a loss is
//!   counted as the end of a job, so an attempt that
//!   retries counts as a retry and not as a start failure or a loss. Each is counted as
//!   its commit lands, so a pass that fails later still counts what it ended. The paths that count them call
//!   a [`MetricsSink`], and do not know the exporter: without `--metrics-listen` the sink is
//!   [`NoMetrics`], which counts nothing.
//! - **Snapshots**, read at each scrape from the state `GET /v1/hosts` and the job service
//!   read ([`scrape`]): the jobs by state, the age of the oldest queued job, the completion
//!   callbacks still pending, and for each host
//!   the CPU and memory booked and free and its whole-host mark, and under
//!   `--placement-curves shadow` and `on` the classes booking by curve and flat and, under `on`,
//!   each curve host's fullest booked slot, read from the placer
//!   ([`js_sched_placer::place::SchedPlacer::curve_view`]); under `shadow` and `on` the memory
//!   shadow: the placements by how a booking that follows the classes' memory curves would have
//!   placed them, and each host's projected memory, its last reading and how often a reading
//!   showed more than the projection ([`js_sched_placer::place::SchedPlacer::memory_shadow_view`]);
//!   nothing in it is booked; and the placement by load, read from
//!   the placer with the registry's hosts ([`js_sched_placer::place::SchedPlacer::telemetry`]):
//!   what admission has done (`js_placement_admissions_total` and its kin, `js_placement_decided_total`
//!   by `key`, `js_load_readings_total` by `result`), how each host is charged, each host's CPU
//!   pressure and OOM events and the hosts kept empty, the ratios of the runs freed while serving
//!   against what they booked (`js_run_*`, histograms with the buckets of
//!   [`js_sched_placer::telemetry::FIT_BOUNDS_PERMILLE`]) and the checks of those runs
//!   (`js_curve_checks_total` by `resource` and `status`).
//!   A scrape holds no lock of the
//!   server's while it renders: the registry's is taken and dropped as `GET /v1/hosts`
//!   takes it, and the job tables are read on the pool for blocking
//!   calls. The parts are read one after another, not in one transaction: a job placed between
//!   two reads can show as queued while its booking already counts.
//!
//! The text names no job, lease, run or token: its labels are a job state, a start failure
//! code, a host's id, a pool's name, a host's mark, a callback's result, a phase of a
//! submit, a decision key, a reading's result, a resource, a check's status, a memory-shadow
//! verdict, an agent's route and a bucket bound,
//! and nothing else. The route takes no
//! token, so it is served only on a private address ([`crate::is_private_address`]), and
//! only to a private peer, as `/status` is.

mod placement;
mod render;
mod scrape;

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::Duration;

use js_callback::{CallbackAnswer, FailureClass};
use js_elastic::{Acquired, Refusal, RefusalCode};
use js_jobs::StartFailure;
use js_sched_placer::engine::TablesFrom;
use js_state::{Clock, StoreTime};
use js_types::{HostId, Monotonic};

pub(crate) use scrape::router;

/// Told of each event the metrics count. Called on the paths that make the event happen, so
/// a method neither blocks nor calls back into the server: it bumps a counter.
pub(crate) trait MetricsSink: Send + Sync + std::fmt::Debug {
    /// A queued job was placed, `waited` after it was submitted.
    fn job_placed(&self, waited: Duration);
    /// A job failed to start, for `failure`.
    fn start_failed(&self, failure: StartFailure);
    /// A job was lost.
    fn job_lost(&self);
    /// A pass the server made over the job tables failed, so its write did not commit.
    fn store_write_failed(&self);
    /// A job pass took `took`.
    fn job_pass_took(&self, took: Duration);
    /// A pass over the queued jobs took the job tables `from` there.
    fn job_tables(&self, from: TablesFrom);
    /// The scaler ran a launch of a machine for the pool `pool`, answered `answer`.
    fn acquire_answered(&self, pool: &str, answer: &Acquired);
    /// The placement refused `host` a queued job for `reason`, one of the names of
    /// `js_sched::RefusalKind::as_str`; by default, nothing is counted.
    fn placement_refused(&self, _host: &HostId, _reason: &'static str) {}
    /// The agent of `host` refused a job's start with the refusal code `code`, or with none
    /// (`code` is then `none`); by default, nothing is counted. The recorder counts it under
    /// the code when it is 1 to 64 characters of `a-z`, `0-9` and `-`, and under `other`
    /// otherwise, so an agent's text never makes a label value of its own choosing.
    fn start_refused(&self, _host: &HostId, _code: &str) {}
    /// A queued job of a curve class waited past the wait bound with no host fitting its
    /// curve, and is tested with its flat number from then on; by default, nothing is counted.
    fn curve_wait_expired(&self) {}
    /// A try of a job's completion callback was answered `answer`, and the answer recorded;
    /// by default, nothing is counted.
    fn callback_answered(&self, _answer: &CallbackAnswer) {}
    /// An attempt of a job ended as `outcome` says and the job's next attempt was queued; by
    /// default, nothing is counted.
    fn job_retried(&self, _outcome: RetryOutcome) {}
    /// A job ended in a way that retries while its retry policy had no attempt left; by
    /// default, nothing is counted.
    fn attempts_exhausted(&self) {}
    /// The phase `phase` of a `SubmitJob` call took `took`; by default, nothing is counted.
    fn submit_phase(&self, _phase: SubmitPhase, _took: Duration) {}
    /// An agent's request on `route` was refused 413 `body-too-large`, its body over the cap;
    /// by default, nothing is counted.
    fn agent_body_too_large(&self, _route: AgentRoute) {}
}

/// An agent route whose body is read whole and held to the cap: the label of
/// `js_agent_bodies_too_large_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentRoute {
    /// `POST /v1/agent/hello`.
    Hello,
    /// `POST /v1/agent/heartbeat`.
    Heartbeat,
}

/// The agent routes, in the order they are written, as the metric spells them.
const AGENT_ROUTES: [&str; 2] = ["hello", "heartbeat"];

fn route_slot(route: AgentRoute) -> usize {
    match route {
        AgentRoute::Hello => 0,
        AgentRoute::Heartbeat => 1,
    }
}

/// A phase of a `SubmitJob` call (`job_api/submit.rs`, steps 1 to 5): the label of
/// `js_submit_phase_seconds`. A call is timed in each phase it reaches, whatever the phase's
/// answer, and in no other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubmitPhase {
    /// The checks of the submitted job, with no I/O.
    Validation,
    /// The read of the job's id in the job tables, for a job with an id.
    Get,
    /// The question whether anything could ever serve the job, for a job not yet held.
    Admission,
    /// Of the insert of the job as queued, the time spent getting connections of the store's
    /// pool (zero for a store with no pool).
    InsertCheckout,
    /// Of the insert of the job as queued, the rest: its reads, its transaction and its commit.
    InsertTransaction,
    /// The placer told of the job the commit recorded, which never waits for a pass that holds
    /// the placer's tables; only when the service places jobs.
    NoteInserted,
}

/// The phases of a submit, in the order they are written, as the metric spells them.
const SUBMIT_PHASES: [&str; 6] = [
    "validation",
    "get",
    "admission",
    "insert-checkout",
    "insert-transaction",
    "note-inserted",
];

fn phase_slot(phase: SubmitPhase) -> usize {
    match phase {
        SubmitPhase::Validation => 0,
        SubmitPhase::Get => 1,
        SubmitPhase::Admission => 2,
        SubmitPhase::InsertCheckout => 3,
        SubmitPhase::InsertTransaction => 4,
        SubmitPhase::NoteInserted => 5,
    }
}

/// How an attempt that retries ended: the label of `js_job_retries_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RetryOutcome {
    /// The attempt failed to start for a cause of its host.
    StartFailed,
    /// The attempt's host was lost.
    HostLost,
}

/// The retry outcomes, in the order they are written, as the metric spells them.
const RETRY_OUTCOMES: [&str; 2] = ["start-failed", "host-lost"];

fn retry_slot(outcome: RetryOutcome) -> usize {
    match outcome {
        RetryOutcome::StartFailed => 0,
        RetryOutcome::HostLost => 1,
    }
}

/// The longest refusal code of an agent that is kept as a metric label value.
const CODE_LABEL_MAX: usize = 64;

/// The label value an agent's refusal `code` is counted under: the code itself when it is 1 to
/// [`CODE_LABEL_MAX`] characters of `a-z`, `0-9` and `-`, and `other` otherwise.
fn code_label(code: &str) -> &str {
    let plain = (1..=CODE_LABEL_MAX).contains(&code.len())
        && code
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    if plain { code } else { "other" }
}

/// Counts nothing, and reads a clock that never moves: the sink and the clock of a server
/// without `--metrics-listen`.
#[derive(Debug)]
pub(crate) struct NoMetrics;

impl MetricsSink for NoMetrics {
    fn job_placed(&self, _: Duration) {}
    fn start_failed(&self, _: StartFailure) {}
    fn job_lost(&self) {}
    fn store_write_failed(&self) {}
    fn job_pass_took(&self, _: Duration) {}
    fn job_tables(&self, _: TablesFrom) {}
    fn acquire_answered(&self, _: &str, _: &Acquired) {}
}

/// The sink it holds, as the placer's job pass reports to it: each placement it committed,
/// each whose commit failed, and where the pass took the job tables from.
pub(crate) struct PassSink<'a>(pub(crate) &'a dyn MetricsSink);

// The pass's trait is named by its path, so that its methods never meet the sink's own in a
// call here.
impl js_sched_placer::engine::jobs::PassMetrics for PassSink<'_> {
    fn job_placed(&self, waited: Duration) {
        self.0.job_placed(waited);
    }

    fn store_write_failed(&self) {
        self.0.store_write_failed();
    }

    fn job_tables(&self, from: TablesFrom) {
        self.0.job_tables(from);
    }

    fn refused(&self, host: &HostId, reason: &'static str) {
        self.0.placement_refused(host, reason);
    }

    fn curve_wait_expired(&self) {
        self.0.curve_wait_expired();
    }
}

impl js_sched_placer::engine::jobs::PassMetrics for NoMetrics {
    fn job_placed(&self, _: Duration) {}
    fn store_write_failed(&self) {}
}

impl Monotonic for NoMetrics {
    fn reading(&self) -> Duration {
        Duration::ZERO
    }
}

/// A sink, with the clock that times what it is told took a while.
#[derive(Clone)]
pub(crate) struct Meter {
    sink: Arc<dyn MetricsSink>,
    monotonic: Arc<dyn Monotonic>,
    /// The same clock, as a store's checkout tally reads one
    /// ([`js_state::postgres::checkout_waits`]).
    tally: Arc<dyn Clock>,
}

/// A monotonic clock read as a store's clock.
struct Tallying(Arc<dyn Monotonic>);

impl Clock for Tallying {
    fn now(&self) -> StoreTime {
        StoreTime::from_duration(self.0.reading())
    }
}

/// The meter of a server without `--metrics-listen`.
static OFF: LazyLock<Meter> =
    LazyLock::new(|| Meter::new(Arc::new(NoMetrics), Arc::new(NoMetrics)));

impl Meter {
    /// `sink`, timing with `monotonic`.
    pub(crate) fn new(sink: Arc<dyn MetricsSink>, monotonic: Arc<dyn Monotonic>) -> Self {
        let tally = Arc::new(Tallying(Arc::clone(&monotonic)));
        Self {
            sink,
            monotonic,
            tally,
        }
    }

    /// The meter that counts nothing.
    pub(crate) fn off() -> &'static Meter {
        &OFF
    }

    /// Where the events go.
    pub(crate) fn sink(&self) -> &dyn MetricsSink {
        self.sink.as_ref()
    }

    /// Runs `work`; what it returned and how long it took by the meter's clock.
    pub(crate) fn timed<T>(&self, work: impl FnOnce() -> T) -> (T, Duration) {
        let began = self.monotonic.reading();
        let done = work();
        (done, self.monotonic.reading().saturating_sub(began))
    }

    /// Awaits `work`; what it gave and how long it took by the meter's clock.
    pub(crate) async fn timed_await<T>(&self, work: impl Future<Output = T>) -> (T, Duration) {
        let began = self.monotonic.reading();
        let done = work.await;
        (done, self.monotonic.reading().saturating_sub(began))
    }

    /// Runs `work` on this thread; what it returned, how long it took by the meter's clock, and
    /// how much of that went to getting connections of a store's pool
    /// ([`js_state::postgres::checkout_waits`]), never more than the whole.
    pub(crate) fn timed_checkouts<T>(&self, work: impl FnOnce() -> T) -> (T, Duration, Duration) {
        let began = self.tally.now();
        let (done, waited) = js_state::postgres::checkout_waits(Arc::clone(&self.tally), work);
        let took = self.tally.now().since(began);
        (done, took, waited.min(took))
    }
}

/// One histogram bucket: its upper bound, inclusive, and that bound as the `le` label says it.
#[derive(Debug)]
struct Bound {
    upto: Duration,
    le: &'static str,
}

/// The buckets of the queue wait: from a second to a day.
const QUEUE_WAIT: &[Bound] = &[
    Bound {
        upto: Duration::from_millis(1_000),
        le: "1",
    },
    Bound {
        upto: Duration::from_millis(5_000),
        le: "5",
    },
    Bound {
        upto: Duration::from_millis(30_000),
        le: "30",
    },
    Bound {
        upto: Duration::from_millis(60_000),
        le: "60",
    },
    Bound {
        upto: Duration::from_millis(300_000),
        le: "300",
    },
    Bound {
        upto: Duration::from_millis(900_000),
        le: "900",
    },
    Bound {
        upto: Duration::from_millis(3_600_000),
        le: "3600",
    },
    Bound {
        upto: Duration::from_millis(14_400_000),
        le: "14400",
    },
    Bound {
        upto: Duration::from_millis(86_400_000),
        le: "86400",
    },
];

/// The buckets of a job pass: from a millisecond to five seconds.
const JOB_PASS: &[Bound] = &[
    Bound {
        upto: Duration::from_millis(1),
        le: "0.001",
    },
    Bound {
        upto: Duration::from_millis(5),
        le: "0.005",
    },
    Bound {
        upto: Duration::from_millis(10),
        le: "0.01",
    },
    Bound {
        upto: Duration::from_millis(50),
        le: "0.05",
    },
    Bound {
        upto: Duration::from_millis(100),
        le: "0.1",
    },
    Bound {
        upto: Duration::from_millis(500),
        le: "0.5",
    },
    Bound {
        upto: Duration::from_millis(1_000),
        le: "1",
    },
    Bound {
        upto: Duration::from_millis(5_000),
        le: "5",
    },
];

/// The buckets of a phase of a submit: from 100 microseconds to a second.
const SUBMIT_PHASE: &[Bound] = &[
    Bound {
        upto: Duration::from_micros(100),
        le: "0.0001",
    },
    Bound {
        upto: Duration::from_micros(250),
        le: "0.00025",
    },
    Bound {
        upto: Duration::from_micros(500),
        le: "0.0005",
    },
    Bound {
        upto: Duration::from_millis(1),
        le: "0.001",
    },
    Bound {
        upto: Duration::from_micros(2_500),
        le: "0.0025",
    },
    Bound {
        upto: Duration::from_millis(5),
        le: "0.005",
    },
    Bound {
        upto: Duration::from_millis(10),
        le: "0.01",
    },
    Bound {
        upto: Duration::from_millis(25),
        le: "0.025",
    },
    Bound {
        upto: Duration::from_millis(50),
        le: "0.05",
    },
    Bound {
        upto: Duration::from_millis(100),
        le: "0.1",
    },
    Bound {
        upto: Duration::from_millis(250),
        le: "0.25",
    },
    Bound {
        upto: Duration::from_secs(1),
        le: "1",
    },
];

/// A histogram of durations over fixed buckets: how many fell in each (not yet cumulative;
/// the last slot is above every bound) and their sum in microseconds.
#[derive(Debug)]
struct Histogram {
    bounds: &'static [Bound],
    counts: Vec<AtomicU64>,
    sum_micros: AtomicU64,
}

impl Histogram {
    fn new(bounds: &'static [Bound]) -> Self {
        Self {
            bounds,
            counts: (0..=bounds.len()).map(|_| AtomicU64::new(0)).collect(),
            sum_micros: AtomicU64::new(0),
        }
    }

    fn observe(&self, value: Duration) {
        let mut bounds = self.bounds.iter();
        let slot = bounds.position(|bound| value <= bound.upto);
        let slot = slot.unwrap_or(self.bounds.len());
        self.counts[slot].fetch_add(1, Ordering::Relaxed);
        let micros = u64::try_from(value.as_micros()).unwrap_or(u64::MAX);
        self.sum_micros.fetch_add(micros, Ordering::Relaxed);
    }

    /// Each bound's `le` with how many fell at or below it, then the count of all of them and
    /// their sum in microseconds.
    fn read(&self) -> (Vec<(&'static str, u64)>, u64, u64) {
        let mut seen = 0;
        let below = self.bounds.iter().zip(&self.counts).map(|(bound, count)| {
            seen += count.load(Ordering::Relaxed);
            (bound.le, seen)
        });
        let below: Vec<_> = below.collect();
        let above: u64 = self
            .counts
            .iter()
            .map(|count| count.load(Ordering::Relaxed))
            .sum();
        (below, above, self.sum_micros.load(Ordering::Relaxed))
    }
}

/// The start failure codes, in the order they are written, as the job service spells them.
const FAILURE_CODES: [&str; 5] = [
    "no-capacity",
    "start-timeout",
    "exclusive-not-ready",
    "agent-refused",
    "input-unavailable",
];

/// Where a pass took the job tables from, in the order they are written, as the metric spells
/// them.
const TABLES_FROM: [&str; 4] = ["store", "kept", "submitted", "changed"];

/// The results of a callback's try, in the order they are written: a 2xx, a 410, a failure,
/// and an address the rule refused.
const CALLBACK_RESULTS: [&str; 4] = ["delivered", "abandoned", "failed", "refused-address"];

/// The word a callback's `answer` is counted and logged under.
pub(crate) fn callback_result(answer: &CallbackAnswer) -> &'static str {
    CALLBACK_RESULTS[callback_slot(answer)]
}

fn callback_slot(answer: &CallbackAnswer) -> usize {
    match answer {
        CallbackAnswer::Delivered => 0,
        CallbackAnswer::Abandoned => 1,
        CallbackAnswer::Failed(FailureClass::Failed) => 2,
        CallbackAnswer::Failed(FailureClass::RefusedAddress) => 3,
    }
}

fn tables_slot(from: TablesFrom) -> usize {
    match from {
        TablesFrom::Store => 0,
        TablesFrom::Kept => 1,
        TablesFrom::Submitted => 2,
        TablesFrom::Changed => 3,
    }
}

fn failure_slot(failure: StartFailure) -> usize {
    match failure {
        StartFailure::NoCapacity => 0,
        StartFailure::StartTimeout => 1,
        StartFailure::ExclusiveNotReady => 2,
        StartFailure::AgentRefused => 3,
        StartFailure::InputUnavailable => 4,
    }
}

/// What the scaler asked of one pool's provider: the launches, and those that failed over.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Acquires {
    asked: u64,
    failed_over: u64,
}

/// Whether `answer` makes the scaler try another offer: a refusal for capacity or quota,
/// which also sets the catalog of the provider to be fetched again
/// ([`crate::Catalogs::acquired`]).
fn fails_over(answer: &Acquired) -> bool {
    matches!(
        answer,
        Acquired::Refused(Refusal {
            code: RefusalCode::Capacity | RefusalCode::Quota,
            ..
        })
    )
}

/// The sink of a server with `--metrics-listen`: the events counted since it started.
#[derive(Debug)]
pub(crate) struct Recorder {
    placed: AtomicU64,
    queue_wait: Histogram,
    start_failures: [AtomicU64; 5],
    lost: AtomicU64,
    store_write_failures: AtomicU64,
    job_pass: Histogram,
    job_tables: [AtomicU64; 4],
    acquires: Mutex<BTreeMap<String, Acquires>>,
    refusals: Mutex<BTreeMap<(String, &'static str), u64>>,
    start_refusals: Mutex<BTreeMap<(String, String), u64>>,
    curve_wait_expiries: AtomicU64,
    retries: [AtomicU64; 2],
    attempts_exhausted: AtomicU64,
    callbacks: [AtomicU64; 4],
    submit_phases: [Histogram; 6],
    bodies_too_large: [AtomicU64; 2],
}

impl Default for Recorder {
    fn default() -> Self {
        Self {
            placed: AtomicU64::new(0),
            queue_wait: Histogram::new(QUEUE_WAIT),
            start_failures: Default::default(),
            lost: AtomicU64::new(0),
            store_write_failures: AtomicU64::new(0),
            job_pass: Histogram::new(JOB_PASS),
            job_tables: Default::default(),
            acquires: Mutex::default(),
            refusals: Mutex::default(),
            start_refusals: Mutex::default(),
            curve_wait_expiries: AtomicU64::new(0),
            retries: Default::default(),
            attempts_exhausted: AtomicU64::new(0),
            callbacks: Default::default(),
            submit_phases: std::array::from_fn(|_| Histogram::new(SUBMIT_PHASE)),
            bodies_too_large: Default::default(),
        }
    }
}

impl MetricsSink for Recorder {
    fn job_placed(&self, waited: Duration) {
        self.placed.fetch_add(1, Ordering::Relaxed);
        self.queue_wait.observe(waited);
    }

    fn start_failed(&self, failure: StartFailure) {
        self.start_failures[failure_slot(failure)].fetch_add(1, Ordering::Relaxed);
    }

    fn job_lost(&self) {
        self.lost.fetch_add(1, Ordering::Relaxed);
    }

    fn store_write_failed(&self) {
        self.store_write_failures.fetch_add(1, Ordering::Relaxed);
    }

    fn job_pass_took(&self, took: Duration) {
        self.job_pass.observe(took);
    }

    fn job_tables(&self, from: TablesFrom) {
        self.job_tables[tables_slot(from)].fetch_add(1, Ordering::Relaxed);
    }

    fn acquire_answered(&self, pool: &str, answer: &Acquired) {
        let mut acquires = self.acquires.lock().unwrap_or_else(PoisonError::into_inner);
        let seen = acquires.entry(pool.to_owned()).or_default();
        seen.asked += 1;
        seen.failed_over += u64::from(fails_over(answer));
    }

    fn placement_refused(&self, host: &HostId, reason: &'static str) {
        let mut refusals = self.refusals.lock().unwrap_or_else(PoisonError::into_inner);
        let seen = refusals
            .entry((host.as_str().to_owned(), reason))
            .or_default();
        *seen += 1;
    }

    fn start_refused(&self, host: &HostId, code: &str) {
        let mut refused = self
            .start_refusals
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let seen = refused
            .entry((host.as_str().to_owned(), code_label(code).to_owned()))
            .or_default();
        *seen += 1;
    }

    fn curve_wait_expired(&self) {
        self.curve_wait_expiries.fetch_add(1, Ordering::Relaxed);
    }

    fn job_retried(&self, outcome: RetryOutcome) {
        self.retries[retry_slot(outcome)].fetch_add(1, Ordering::Relaxed);
    }

    fn attempts_exhausted(&self) {
        self.attempts_exhausted.fetch_add(1, Ordering::Relaxed);
    }

    fn callback_answered(&self, answer: &CallbackAnswer) {
        self.callbacks[callback_slot(answer)].fetch_add(1, Ordering::Relaxed);
    }

    fn submit_phase(&self, phase: SubmitPhase, took: Duration) {
        self.submit_phases[phase_slot(phase)].observe(took);
    }

    fn agent_body_too_large(&self, route: AgentRoute) {
        self.bodies_too_large[route_slot(route)].fetch_add(1, Ordering::Relaxed);
    }
}

/// The text a scrape of no job tables and no hosts answers, with the events `recorder` counted.
#[cfg(test)]
pub(crate) fn rendered(recorder: &Recorder) -> String {
    let seen = render::Snapshot {
        jobs: None,
        hosts: Vec::new(),
        curves: None,
        telemetry: None,
        memory: None,
    };
    render::render(&seen, recorder)
}

impl Recorder {
    /// The callback answers counted, by result, in the order of [`CALLBACK_RESULTS`].
    #[cfg(test)]
    pub(crate) fn callbacks(&self) -> [u64; 4] {
        self.callbacks
            .each_ref()
            .map(|count| count.load(Ordering::Relaxed))
    }

    /// The starts an agent refused, by host and refusal code label.
    fn start_refusals(&self) -> BTreeMap<(String, String), u64> {
        self.start_refusals
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The refusals counted, by host and reason.
    fn refusals(&self) -> BTreeMap<(String, &'static str), u64> {
        self.refusals
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The pools the scaler asked a provider for, each with what it asked.
    fn acquires(&self) -> BTreeMap<String, Acquires> {
        self.acquires
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[cfg(test)]
#[path = "metrics/tests.rs"]
mod tests;

#[cfg(test)]
#[path = "metrics/events_tests.rs"]
mod events_tests;

#[cfg(test)]
#[path = "metrics/retry_tests.rs"]
mod retry_tests;

#[cfg(test)]
#[path = "metrics/telemetry_tests.rs"]
mod telemetry_tests;
