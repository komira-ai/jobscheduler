//! What the configuration decided for the agents that register hosts: [`AgentSetup`], the
//! clock and id source its job store is opened on ([`Config::with_job_sources`]).

use std::sync::Arc;
use std::time::Duration;

use js_registry::PoolConfig;
use js_types::ProviderId;

use super::{Config, CpuSlack, Hosts};
use crate::agent_tokens::AgentTokens;
use crate::job_expiry::JobLimits;
use crate::job_store::{JobSources, JobStoreSetup, ServerIds};
use crate::rate::HelloLimit;

/// What `--agent-pool-config`, `--agent-token-file` (when given) and the hello limit
/// decided.
pub(crate) struct AgentSetup {
    pub(crate) tokens: AgentTokens,
    pub(crate) pool: PoolConfig,
    /// The one provider the mint route accepts, when the configuration names it.
    pub(crate) mint_provider: Option<String>,
    pub(crate) hello_limit: HelloLimit,
    /// `--require-host-credential`.
    pub(crate) require_host_credential: bool,
    /// `--cpu-slack`, or the engine's default.
    pub(crate) cpu_slack: CpuSlack,
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

impl Config {
    /// The same configuration with the job store (when the job service is on) stamping its
    /// rows with `clock` and minting its ids from `ids`, in place of the system's clock and
    /// random source: how a simulation makes its job and allocation ids, and the times it
    /// stores, a function of its seed and its virtual clock. Without agent registration
    /// there is no job store, and the configuration is unchanged.
    pub fn with_job_sources(
        mut self,
        clock: Arc<dyn js_state::Clock>,
        ids: Arc<dyn js_jobs::IdSource>,
    ) -> Self {
        if let Hosts::Layered { agents, .. } = &mut self.hosts {
            agents.job_sources = JobSources {
                clock,
                ids: ServerIds::new(ids),
            };
        }
        self
    }
}
