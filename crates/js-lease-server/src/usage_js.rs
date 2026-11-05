//! The sink the server runs on: the one place that decides how usage is measured and
//! kept. It is [`js_usage`]'s recorder and node series over a [`js_state::Store`].
//!
//! A reading goes into the [`Recorder`], which judges it (a lying node, a resend, a
//! counter that went back) and keeps the node's minute being built. A flush moves the
//! closed wall minutes into a [`NodeBuffer`] and writes the buffer through
//! [`NodeSeries`]; a minute leaves the buffer only when its write succeeded. A prune
//! removes hours past [`js_usage::NODE_RETENTION_MINUTES`].
//!
//! The profile side: a run that ended cleanly goes into the [`ProfileStore`] under its
//! key, which refits the key's curve; the daily prune tells the curves what day it is and
//! writes the days keys were last used. Reading a stored series or profile back is the
//! read routes' part, in `usage_api`.
//!
//! The tail counters: the sink keeps a [`StoreTailSink`] over the same store, which it hands
//! the registry ([`UsageSink::tail`]); the registry hands it each change once its lock is
//! released, which only joins the counts waiting to be written, and every flush writes
//! them. A write that fails keeps them for the next flush, is counted, and is logged once
//! per run of failures.
//!
//! The history side: the record of each ended allocation is written under its key in
//! [`EndedHistory`] (once: a row already there is kept), and the daily prune also removes the
//! records older than `--history-retention-days`.
//!
//! One mutex guards the recorder and the buffer, and is held across the store's write in
//! a flush: readings wait for it, which is cheap (a write of at most a day of minutes
//! per node) and never happens on an async worker, since the server calls the sink on the
//! pool for blocking calls.

use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use js_registry::{ERROR_EDGES_MS, Registry};
use js_state::sqlite::{Options, SqliteStore};
use js_state::{Store, SystemClock};
use js_types::HostId;
use js_types::sched::WorkKey;
use js_usage::{
    EndedHistory, EndedRow, KeyProfile, NodeBuffer, NodeSeries, ProfileCurves, ProfileError,
    ProfileKey, ProfileStore, Recorder, RunSeries, StoreTailSink, TailFlush, TailSink,
};
use tracing::{info, warn};

use crate::error::StartupError;
use crate::usage::{
    HostLimits, KeySource, LoadIn, NodeRow, Observation, RunEnd, SinkCounters, UsageFault,
    UsageSink,
};
use crate::usage_setup::DEFAULT_HISTORY_RETENTION_DAYS;

/// How admission finds the profile name of a work key: the caller supplies it, since the
/// name is the caller's (a key of its own estimator), not something the server derives from
/// the work key. `None` charges the key flat.
pub(crate) type NameMap = Arc<dyn Fn(&WorkKey) -> Option<ProfileKey> + Send + Sync>;

/// The map of a server whose caller has named none: no work key has a profile.
pub(crate) fn no_names() -> NameMap {
    Arc::new(|_| None)
}

/// Where the usage store is and how large the profile part of it may grow.
#[derive(Clone)]
pub(crate) struct StoreSetup {
    pub(crate) db: PathBuf,
    pub(crate) profile_store_bytes: u64,
    /// How many days a record of an ended allocation is kept (`--history-retention-days`).
    pub(crate) history_retention_days: u64,
    /// How admission's curve reads find the profile of a work key.
    pub(crate) names: NameMap,
}

/// The sink over the SQLite file `setup` names. A file that cannot be opened (another
/// process holds it, a network filesystem, not a state store) is a refused start naming
/// `--usage-db`.
pub(crate) fn open(setup: &StoreSetup, today: u64) -> Result<Arc<dyn UsageSink>, StartupError> {
    let clock = Arc::new(SystemClock::default());
    let store = SqliteStore::open(&setup.db, clock, Options::default())
        .map_err(StartupError::because("--usage-db"))?;
    sink_over(Arc::new(store), setup, today)
}

/// The sink over an opened `store`; a store whose profiles cannot be read is a refused
/// start naming `--usage-db`.
fn sink_over(
    store: Arc<dyn Store>,
    setup: &StoreSetup,
    today: u64,
) -> Result<Arc<dyn UsageSink>, StartupError> {
    let usage = NodeUsage::over(store, setup.profile_store_bytes, today, setup.names.clone())
        .map_err(StartupError::because("--usage-db"))?
        .with_history_retention_days(setup.history_retention_days);
    info!(
        db = %setup.db.display(),
        profile_store_bytes = setup.profile_store_bytes,
        history_retention_days = setup.history_retention_days,
        "usage is recorded"
    );
    Ok(Arc::new(usage))
}

struct Inner {
    recorder: Recorder,
    buffer: NodeBuffer,
    profiles: ProfileStore,
}

/// The key source the server runs on: the profile name the client supplied when it queued
/// the run, held by the registry beside the run. A run queued without a name has no key, and
/// is counted as a run without one.
pub(crate) struct RegistryKeys(pub(crate) Registry);

impl KeySource for RegistryKeys {
    fn key_of(&self, run_id: &str) -> Option<ProfileKey> {
        self.0.profile_of(run_id)
    }
}

/// Minutes in a day.
const MINUTES_PER_DAY: u64 = 1440;

/// Milliseconds in a day.
const MILLIS_PER_DAY: u64 = 86_400_000;

/// Milliseconds in a minute.
const MILLIS_PER_MINUTE: u64 = 60_000;

/// The node series over `store`.
pub(crate) struct NodeUsage {
    store: Arc<dyn Store>,
    curves: Arc<ProfileCurves>,
    inner: Mutex<Inner>,
    /// The registry's tail counters, in the same store.
    tail: Arc<StoreTailSink>,
    /// Whether the last write of the tail counters failed: a failure is logged when it
    /// starts a run of them, and the recovery when it ends one.
    tail_failing: AtomicBool,
    /// How long a record of an ended allocation is kept, in milliseconds.
    history_retention_ms: u64,
}

fn fault(err: impl std::fmt::Display) -> UsageFault {
    UsageFault(err.to_string())
}

impl NodeUsage {
    /// The sink over `store`, keeping at most `budget_bytes` of profiles; `today` is the
    /// day (days since the epoch) the curves start from.
    pub(crate) fn over(
        store: Arc<dyn Store>,
        budget_bytes: u64,
        today: u64,
        names: NameMap,
    ) -> Result<Self, ProfileError> {
        let curves = Arc::new(ProfileCurves::new(Box::new(move |key| names(key))));
        let profiles = ProfileStore::open(store.clone(), curves.clone(), budget_bytes, today)?;
        Ok(Self {
            tail: Arc::new(StoreTailSink::new(store.clone(), ERROR_EDGES_MS)),
            store,
            curves,
            inner: Mutex::new(Inner {
                recorder: Recorder::new(),
                buffer: NodeBuffer::new(),
                profiles,
            }),
            tail_failing: AtomicBool::new(false),
            history_retention_ms: DEFAULT_HISTORY_RETENTION_DAYS * MILLIS_PER_DAY,
        })
    }

    /// The same sink, keeping a record of an ended allocation for `days` days
    /// ([`DEFAULT_HISTORY_RETENTION_DAYS`] unless set).
    pub(crate) fn with_history_retention_days(self, days: u64) -> Self {
        Self {
            history_retention_ms: days.saturating_mul(MILLIS_PER_DAY),
            ..self
        }
    }

    fn inner(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Writes the tail counts that wait, and says what is to be logged: the first failure
    /// of a run of them, and the write that ends the run. A failure costs the stored
    /// counters only: the sink counts it and keeps or drops the counts as it says. A flush
    /// with nothing waiting asks the store nothing, so it neither ends a run nor starts one.
    fn flush_tail(&self) -> Option<TailNote> {
        let set = |failing| self.tail_failing.swap(failing, Ordering::SeqCst);
        match self.tail.flush() {
            Ok(TailFlush::Idle) => None,
            Ok(TailFlush::Written) => set(false).then_some(TailNote::Recovered),
            Err(error) => (!set(true)).then(|| TailNote::Failing(error.to_string())),
        }
    }
}

/// What a flush of the tail counters logs.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TailNote {
    /// The first failed write of a run of them, and why it failed.
    Failing(String),
    /// A write that landed after a run of failed ones.
    Recovered,
}

/// Logs `note`.
fn log_tail(note: TailNote) {
    match note {
        TailNote::Failing(error) => {
            warn!(%error, "cannot write the tail counters; they wait for the next flush");
        }
        TailNote::Recovered => info!("the tail counters are written again"),
    }
}

impl UsageSink for NodeUsage {
    fn observe(
        &self,
        host: &HostId,
        epoch: u64,
        load: &LoadIn,
        limits: HostLimits,
        now_minute: u64,
    ) -> Observation {
        let (observed, fresh) = self
            .inner()
            .recorder
            .observe(host, epoch, load, limits, now_minute);
        Observation { observed, fresh }
    }

    fn run_ended(&self, end: &RunEnd, now_minute: u64) -> Result<(), UsageFault> {
        let mut inner = self.inner();
        let day = now_minute / MINUTES_PER_DAY;
        let series: &RunSeries = &end.series;
        inner
            .profiles
            .run_ended(&end.key, series, day)
            .map(drop)
            .map_err(fault)
    }

    fn flush(&self, now_minute: u64) -> Result<Vec<(HostId, NodeRow)>, UsageFault> {
        self.flush_tail().into_iter().for_each(log_tail);
        let mut inner = self.inner();
        let due = inner.recorder.node_minutes_due(now_minute);
        for (host, row) in &due {
            inner.buffer.push(host.clone(), *row);
        }
        let series = NodeSeries::new(&*self.store);
        inner.buffer.flush(&series).map_err(fault)?;
        Ok(due)
    }

    fn prune(&self, now_minute: u64) -> Result<(), UsageFault> {
        let series = NodeSeries::new(&*self.store);
        series.prune(now_minute).map_err(fault)?;
        let now_ms = now_minute.saturating_mul(MILLIS_PER_MINUTE);
        let history = EndedHistory::new(&*self.store);
        history
            .prune(now_ms, self.history_retention_ms)
            .map_err(fault)?;
        let mut inner = self.inner();
        inner
            .profiles
            .curves()
            .set_today(now_minute / MINUTES_PER_DAY);
        inner.profiles.persist_touches().map(drop).map_err(fault)
    }

    fn node_rows(
        &self,
        host: &HostId,
        from: u64,
        to: u64,
        limit: NonZeroUsize,
    ) -> Result<Vec<NodeRow>, UsageFault> {
        NodeSeries::new(&*self.store)
            .range(host, from, to, limit)
            .map_err(fault)
    }

    fn key_profile(&self, key: &ProfileKey) -> Result<Option<KeyProfile>, UsageFault> {
        self.inner().profiles.profile(key).map_err(fault)
    }

    fn curves(&self) -> Option<Arc<ProfileCurves>> {
        Some(self.curves.clone())
    }

    fn tail(&self) -> Option<Arc<dyn TailSink>> {
        Some(self.tail.clone())
    }

    fn allocation_ended(&self, row: &EndedRow) -> Result<(), UsageFault> {
        let history = EndedHistory::new(&*self.store);
        history.put(row).map(drop).map_err(fault)
    }

    fn ended_rows(
        &self,
        from_ms: u64,
        to_ms: u64,
        limit: NonZeroUsize,
    ) -> Result<Vec<EndedRow>, UsageFault> {
        let history = EndedHistory::new(&*self.store);
        let read = history.range(from_ms, to_ms, limit).map_err(fault)?;
        Ok(read.rows)
    }

    fn counters(&self) -> SinkCounters {
        let counters = self.inner().recorder.counters().clone();
        SinkCounters {
            dropped: counters.dropped,
            minutes_deduped: counters.minutes_deduped,
        }
    }
}

#[cfg(test)]
mod tests;
