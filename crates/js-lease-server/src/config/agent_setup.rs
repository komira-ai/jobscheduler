//! What the configuration decided for the agents that register hosts: [`AgentSetup`], the
//! clock and id source its job store is opened on ([`Config::with_job_sources`]).

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use js_registry::PoolConfig;
use js_sched_placer::sizes::ColdSize;
use js_types::ProviderId;

use super::{Config, CpuSlack};
use crate::agent_tokens::{AgentTokens, PoolTokens};
use crate::error::StartupError;
use crate::job_expiry::JobLimits;
use crate::job_store::{JobSources, JobStoreSetup, ServerIds};
use crate::rate::HelloLimit;

/// What `--agent-pool-config`, `--agent-token-file` (when given) and the hello limit
/// decided.
pub(crate) struct AgentSetup {
    /// The first pool's shared join tokens.
    pub(crate) tokens: AgentTokens,
    /// The first pool: the one the registry is named for, whose whole-node settings and
    /// timing of restored hosts are the server's.
    pub(crate) pool: PoolConfig,
    /// The pools after the first, each with its shared join tokens, in the order given:
    /// a later `--agent-pool-config`, or a later joined `[[pool]]`.
    pub(crate) more: Vec<MorePool>,
    /// The providers the mint route accepts, when the configuration names them; empty, any.
    pub(crate) mint_provider: BTreeSet<String>,
    pub(crate) hello_limit: HelloLimit,
    /// `--require-host-credential`.
    pub(crate) require_host_credential: bool,
    /// `--cpu-slack`, or the engine's default.
    pub(crate) cpu_slack: CpuSlack,
    /// `--cold-cpu-millis` and `--cold-memory-bytes`, or `server.cold_cpu_millis` and
    /// `server.cold_memory_bytes`: what a job that books nothing books while its class has no
    /// estimate (each absent one a quarter of the largest host).
    pub(crate) cold: ColdSize,
    /// `server.max_lease_wall`: the one up to which the placement engine trusts a wall time
    /// for backfill.
    pub(crate) max_lease_wall: Option<Duration>,
    /// The pool's `max_queue_wait` and `start_timeout`, which the tick's job pass enforces
    /// (their defaults for a pool that does not set them, and for `--agent-pool-config`).
    pub(crate) job_limits: JobLimits,
    /// The job store's setup when the job service is on; the agent side then opens it.
    pub(crate) jobs: Option<JobStoreSetup>,
    /// The clock and id source the job store is opened on: the system's, unless
    /// [`crate::Config::with_job_sources`] gave others.
    pub(crate) job_sources: JobSources,
    /// The machine providers of the pool's Default platform, first to last, as the bindings of
    /// its hosts name them ([`crate::sched::rank::order_of`]); `None` when the configuration
    /// names no platform for the pool, and the order is `static`, then the pool's launched
    /// hosts ([`crate::sched::rank::static_then`]).
    pub(crate) providers: Option<Vec<ProviderId>>,
}

/// A pool after the first: what registers into it and its shared join tokens.
pub(crate) struct MorePool {
    pub(crate) pool: PoolConfig,
    pub(crate) tokens: PoolTokens,
}

/// Refuses `pool`, which `place` names, as a pool after `first` and `before`: its name is
/// one of theirs (a hello could not say which it joins); its `memory_schedulable_percent` is
/// not the first pool's (the placement engine books one share of every host's memory); or it
/// has whole-node settings (the server's are the first pool's, so a later pool's would be
/// read by nothing: its class must not be `whole-node`).
pub(crate) fn check_more(
    first: &PoolConfig,
    before: &[MorePool],
    (pool, place): (&PoolConfig, &str),
) -> Result<(), StartupError> {
    let mut earlier = std::iter::once(first).chain(before.iter().map(|more| &more.pool));
    if earlier.any(|earlier| earlier.name() == pool.name()) {
        return Err(StartupError::at(
            place,
            "name: also the name of an earlier pool",
        ));
    }
    let percent = first.memory_percent().get();
    if pool.memory_percent().get() != percent {
        let reason = format!(
            "memory_schedulable_percent must be the first pool's ({percent}): the placement engine books one share of every host's memory"
        );
        return Err(StartupError::at(place, reason));
    }
    if pool.whole_node().is_some() {
        let reason = "whole-node settings are the first pool's alone: give this pool a class that is not whole-node, and no whole-node keys";
        return Err(StartupError::at(place, reason));
    }
    Ok(())
}

impl Config {
    /// The same configuration with the job store (when the job service is on) stamping its
    /// rows with `clock` and minting its ids from `ids`, in place of the system's clock and
    /// random source: how a simulation makes its job and allocation ids, and the times it
    /// stores, a function of its seed and its virtual clock. Without the job service there
    /// is no job store, and the sources go unused.
    pub fn with_job_sources(
        mut self,
        clock: Arc<dyn js_state::Clock>,
        ids: Arc<dyn js_jobs::IdSource>,
    ) -> Self {
        if let Some(store) = self.store.as_mut() {
            store.clock = crate::config::StoreClock(Some(Arc::clone(&clock)));
        }
        self.agents.job_sources = JobSources {
            clock,
            ids: ServerIds::new(ids),
        };
        self
    }
}
