//! The sink the server runs on: the one place that decides how usage is measured and
//! kept. It is [`js_usage`]'s recorder and node series over a [`js_state::Store`].
//!
//! A reading goes into the [`Recorder`], which judges it (a lying node, a resend, a
//! counter that went back) and keeps the node's minute being built. A flush moves the
//! closed wall minutes into a [`NodeBuffer`] and writes the buffer through
//! [`NodeSeries`]; a minute leaves the buffer only when its write succeeded. A prune
//! removes hours past [`js_usage::NODE_RETENTION_MINUTES`].
//!
//! The profile side (learning a cost curve from a run that ended cleanly) is not here
//! yet: [`UsageSink::run_ended`] takes the run and keeps nothing. Reading a stored
//! series back is the read routes' part, in `usage_api`.
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
use js_usage::{NodeBuffer, NodeSeries, Recorder};
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
pub(crate) fn open(setup: &StoreSetup) -> Result<Arc<dyn UsageSink>, StartupError> {
    let clock = Arc::new(SystemClock::default());
    let store = SqliteStore::open(&setup.db, clock, Options::default())
        .map_err(|err| StartupError::new("--usage-db", err))?;
    info!(
        db = %setup.db.display(),
        profile_store_bytes = setup.profile_store_bytes,
        "usage is recorded"
    );
    Ok(Arc::new(NodeUsage::over(Box::new(store))))
}

struct Inner {
    recorder: Recorder,
    buffer: NodeBuffer,
}

/// The node series over `store`.
pub(crate) struct NodeUsage {
    store: Box<dyn Store>,
    inner: Mutex<Inner>,
}

fn fault(err: impl std::fmt::Display) -> UsageFault {
    UsageFault(err.to_string())
}

impl NodeUsage {
    /// The sink over `store`.
    pub(crate) fn over(store: Box<dyn Store>) -> Self {
        Self {
            store,
            inner: Mutex::new(Inner {
                recorder: Recorder::new(),
                buffer: NodeBuffer::new(),
            }),
        }
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

    fn run_ended(&self, _: &RunEnd, _: u64) -> Result<(), UsageFault> {
        Ok(())
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
        series.prune(now_minute).map(drop).map_err(fault)
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
