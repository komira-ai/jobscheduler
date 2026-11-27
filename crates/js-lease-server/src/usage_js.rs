//! The sink the server runs on: the one place that decides how usage is measured and
//! kept. It is [`js_usage`]'s recorder and node series over a [`js_state::Store`].
//!
//! A reading goes into the [`Recorder`], which judges it (a lying node, a resend, a
//! counter that went back) and keeps the node's minute being built. A flush moves the
//! closed wall minutes into a [`NodeBuffer`] and writes the buffer through
//! [`NodeSeries`], as this instance ([`InstanceTag`]): a minute another instance sharing the
//! store wrote first is kept beside its part, not over it. A minute leaves the buffer only
//! when its write succeeded. A prune removes hours past [`js_usage::NODE_RETENTION_MINUTES`].
//!
//! The daily prune has two halves. The deletes of the shared rows (the node hours and the
//! history records past retention) are a duty of the servers sharing the job store: the
//! prune runs them only when it claims `periodic/usage-prune`
//! ([`js_jobs::JobStore::claim_duty`]) for [`USAGE_PRUNE_PERIOD`], so one server deletes in a
//! period and the others pass. A server whose usage rows are not in the job store (its own
//! usage file, or no job service) is their one writer and deletes at each prune. The other
//! half is this instance's own (the curves' day and the days its keys were used) and runs at
//! every prune of every server. A server that claims the deletes and then fails one leaves
//! them to the next period: rows past retention stay a day longer, and nothing is lost.
//!
//! The profile side: a run that ended cleanly goes into the [`ProfileStore`] under its
//! key, which refits the key's curve; the daily prune tells the curves what day it is and
//! writes the days keys were last used. Reading a stored series or profile back is the
//! read routes' part, in `usage_api`.
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
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use js_state::sqlite::{Options, SqliteStore};
use js_state::{Store, SystemClock};
use js_types::HostId;
use js_types::sched::WorkKey;
use js_usage::store::InstanceTag;
use js_usage::{
    EndedHistory, EndedPage, EndedQuery, EndedRow, KeyProfile, NodeBuffer, NodeSeries,
    ProfileCurves, ProfileError, ProfileKey, ProfileStore, Recorder, RunSeries,
};
use tracing::info;

use crate::error::StartupError;
use crate::job_store::ServerJobs;
use crate::usage::{
    HostLimits, LoadIn, NodeRow, Observation, RunEnd, SinkCounters, UsageFault, UsageSink,
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

/// The periodic duty of the deletes of the usage prune, as the servers claim it.
const USAGE_PRUNE: &str = "usage-prune";

/// How long a claim of the usage prune keeps the other servers off it: a day less an hour, so
/// that the prune of each server's next day, run at its first tick of the day, finds the
/// claim of the day before due though the servers' clocks and ticks are some minutes apart.
pub(crate) const USAGE_PRUNE_PERIOD: Duration = Duration::from_secs(23 * 3600);

/// Where the usage store is and how large the profile part of it may grow.
#[derive(Clone)]
pub(crate) struct StoreSetup {
    pub(crate) db: PathBuf,
    pub(crate) profile_store_bytes: u64,
    /// How many days a record of an ended allocation is kept (`--history-retention-days`).
    pub(crate) history_retention_days: u64,
    /// How admission's curve reads find the profile of a work key.
    pub(crate) names: NameMap,
    /// The name of this instance's own rows of the node series.
    pub(crate) instance: InstanceTag,
    /// The job store whose `usage-prune` duty the prune's deletes are claimed by, when the
    /// usage rows are in it; `None` deletes at every prune.
    pub(crate) prune_duty: Option<Arc<ServerJobs>>,
}

/// The sink over the state store `kept` when there is one (the usage rows are keys of the
/// store the rest of the server's state is in), else over the SQLite file `setup` names. A
/// file that cannot be opened (another process holds it, a network filesystem, not a state
/// store) is a refused start naming `--usage-db`.
pub(crate) fn open(
    setup: &StoreSetup,
    kept: Option<Arc<dyn Store>>,
    today: u64,
) -> Result<Arc<dyn UsageSink>, StartupError> {
    let store = match kept {
        Some(store) => store,
        None => {
            let clock = Arc::new(SystemClock::default());
            let file = SqliteStore::open(&setup.db, clock, Options::default())
                .map_err(StartupError::because("--usage-db"))?;
            Arc::new(file)
        }
    };
    sink_over(store, setup, today)
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
        .with_history_retention_days(setup.history_retention_days)
        .writing_as(setup.instance.clone())
        .with_prune_duty(setup.prune_duty.clone());
    info!(
        db = %setup.db.display(),
        profile_store_bytes = setup.profile_store_bytes,
        history_retention_days = setup.history_retention_days,
        instance = %setup.instance,
        prune_claimed = setup.prune_duty.is_some(),
        "usage is recorded"
    );
    Ok(Arc::new(usage))
}

struct Inner {
    recorder: Recorder,
    buffer: NodeBuffer,
    profiles: ProfileStore,
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
    /// How long a record of an ended allocation is kept, in milliseconds.
    history_retention_ms: u64,
    /// The instance the node series is written as; `None` writes it as the one writer.
    instance: Option<InstanceTag>,
    /// The job store the prune's deletes claim the `usage-prune` duty of; `None` deletes at
    /// every prune.
    prune_duty: Option<Arc<ServerJobs>>,
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
            store,
            curves,
            inner: Mutex::new(Inner {
                recorder: Recorder::new(),
                buffer: NodeBuffer::new(),
                profiles,
            }),
            history_retention_ms: DEFAULT_HISTORY_RETENTION_DAYS * MILLIS_PER_DAY,
            instance: None,
            prune_duty: None,
        })
    }

    /// The same sink, writing the node series as the instance `instance`.
    pub(crate) fn writing_as(self, instance: InstanceTag) -> Self {
        Self {
            instance: Some(instance),
            ..self
        }
    }

    /// The same sink, its prune's deletes claimed by the `usage-prune` duty of `jobs` when
    /// there is one.
    pub(crate) fn with_prune_duty(self, jobs: Option<Arc<ServerJobs>>) -> Self {
        Self {
            prune_duty: jobs,
            ..self
        }
    }

    /// The node series over the store, as this instance when it has a name.
    fn series(&self) -> NodeSeries<'_> {
        let series = NodeSeries::new(&*self.store);
        match &self.instance {
            Some(instance) => series.with_instance(instance),
            None => series,
        }
    }

    /// Whether this server runs the prune's deletes now: always without a duty, else when it
    /// claims the `usage-prune` duty for [`USAGE_PRUNE_PERIOD`].
    fn prune_claimed(&self) -> Result<bool, UsageFault> {
        match &self.prune_duty {
            None => Ok(true),
            Some(jobs) => jobs
                .claim_duty(USAGE_PRUNE, USAGE_PRUNE_PERIOD)
                .map_err(fault),
        }
    }

    /// The prune's deletes of the shared rows: the node hours past retention and the history
    /// records older than `--history-retention-days`, when this server claims them.
    fn prune_store(&self, now_minute: u64) -> Result<(), UsageFault> {
        if !self.prune_claimed()? {
            return Ok(());
        }
        let series = NodeSeries::new(&*self.store);
        series.prune(now_minute).map_err(fault)?;
        let now_ms = now_minute.saturating_mul(MILLIS_PER_MINUTE);
        let history = EndedHistory::new(&*self.store);
        history
            .prune(now_ms, self.history_retention_ms)
            .map(drop)
            .map_err(fault)
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
        let mut inner = self.inner();
        let due = inner.recorder.node_minutes_due(now_minute);
        for (host, row) in &due {
            inner.buffer.push(host.clone(), *row);
        }
        inner.buffer.flush(&self.series()).map_err(fault)?;
        Ok(due)
    }

    /// This instance's half first (the curves' day and the days its keys were used), then the
    /// deletes of the shared rows when this server claims them; the first failure is
    /// returned, and a failure of one half does not skip the other.
    fn prune(&self, now_minute: u64) -> Result<(), UsageFault> {
        let mine = {
            let mut inner = self.inner();
            inner
                .profiles
                .curves()
                .set_today(now_minute / MINUTES_PER_DAY);
            inner.profiles.persist_touches().map(drop).map_err(fault)
        };
        let shared = self.prune_store(now_minute);
        mine.and(shared)
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

    fn ended_rows_after(&self, query: &EndedQuery) -> Result<EndedPage, UsageFault> {
        EndedHistory::new(&*self.store).page(query).map_err(fault)
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
