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
//! One mutex guards the recorder and the buffer, and is held across the store's write in
//! a flush: readings wait for it, which is cheap (a write of at most a day of minutes
//! per node) and never happens on an async worker, since the server calls the sink on the
//! pool for blocking calls.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use js_state::sqlite::{Options, SqliteStore};
use js_state::{Store, SystemClock};
use js_types::HostId;
use js_types::profile_key::profile_key;
use js_types::sched::WorkKey;
use js_usage::{
    NodeBuffer, NodeSeries, ProfileCurves, ProfileError, ProfileKey, ProfileStore, Recorder,
    RunSeries,
};
use tracing::info;

use crate::error::StartupError;
use crate::usage::{
    HostLimits, LoadIn, NodeRow, Observation, RunEnd, SinkCounters, UsageFault, UsageSink,
};

/// Where the usage store is and how large the profile part of it may grow.
#[derive(Debug, Clone)]
pub(crate) struct StoreSetup {
    pub(crate) db: PathBuf,
    pub(crate) profile_store_bytes: u64,
}

/// The sink over the SQLite file `setup` names. A file that cannot be opened (another
/// process holds it, a network filesystem, not a state store) is a refused start naming
/// `--usage-db`.
pub(crate) fn open(setup: &StoreSetup, today: u64) -> Result<Arc<dyn UsageSink>, StartupError> {
    let clock = Arc::new(SystemClock::default());
    let store = SqliteStore::open(&setup.db, clock, Options::default())
        .map_err(|err| StartupError::new("--usage-db", err))?;
    let store: Arc<dyn Store> = Arc::new(store);
    let usage = NodeUsage::over(store, setup.profile_store_bytes, today)
        .map_err(|err| StartupError::new("--usage-db", err))?;
    info!(
        db = %setup.db.display(),
        profile_store_bytes = setup.profile_store_bytes,
        "usage is recorded"
    );
    Ok(Arc::new(usage))
}

struct Inner {
    recorder: Recorder,
    buffer: NodeBuffer,
    profiles: ProfileStore,
}

/// The profile name of an action key (see [`profile_key`]), for admission's curves.
pub(crate) fn key_map(key: &WorkKey) -> Option<ProfileKey> {
    ProfileKey::parse(&profile_key(key)).ok()
}

/// Minutes in a day.
const MINUTES_PER_DAY: u64 = 1440;

/// The node series over `store`.
pub(crate) struct NodeUsage {
    store: Arc<dyn Store>,
    curves: Arc<ProfileCurves>,
    inner: Mutex<Inner>,
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
    ) -> Result<Self, ProfileError> {
        let curves = Arc::new(ProfileCurves::new(Box::new(key_map)));
        let profiles = ProfileStore::open(store.clone(), curves.clone(), budget_bytes, today)?;
        Ok(Self {
            store,
            curves,
            inner: Mutex::new(Inner {
                recorder: Recorder::new(),
                buffer: NodeBuffer::new(),
                profiles,
            }),
        })
    }

    fn inner(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The stored rows of `host` from minute `from` to `to`, at most `limit`.
    #[cfg(test)]
    fn range(&self, host: &HostId, from: u64, to: u64) -> Vec<NodeRow> {
        let limit = std::num::NonZeroUsize::new(100).unwrap();
        NodeSeries::new(&*self.store)
            .range(host, from, to, limit)
            .unwrap()
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
        let series = NodeSeries::new(&*self.store);
        inner.buffer.flush(&series).map_err(fault)?;
        Ok(due)
    }

    fn prune(&self, now_minute: u64) -> Result<(), UsageFault> {
        let series = NodeSeries::new(&*self.store);
        series.prune(now_minute).map_err(fault)?;
        let mut inner = self.inner();
        inner
            .profiles
            .curves()
            .set_today(now_minute / MINUTES_PER_DAY);
        inner.profiles.persist_touches().map(drop).map_err(fault)
    }

    fn curves(&self) -> Option<Arc<ProfileCurves>> {
        Some(self.curves.clone())
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
