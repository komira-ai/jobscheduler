//! What the configuration decided for the agents that register hosts: [`AgentSetup`], the
//! clock and id source its job store is opened on ([`Config::with_job_sources`]), and what
//! the registered pool's file decides about whole-node leases ([`with_pool_settings`]).

use std::sync::Arc;
use std::time::Duration;

use js_lease::Settings;
use js_registry::{PoolConfig, WholeNodeConfig};

use super::{Config, CpuSlack, Hosts, Placement};
use crate::agent_tokens::AgentTokens;
use crate::error::StartupError;
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
    /// `--placement`.
    pub(crate) placement: Placement,
    /// `--estimate-feed`.
    pub(crate) estimate_feed: bool,
    /// `--cpu-slack`, or the engine's default: read by `--placement js-sched` only.
    pub(crate) cpu_slack: CpuSlack,
    /// `server.max_lease_wall`: the ceiling the lease table holds a lease's wall time to,
    /// and the one up to which `--placement js-sched` trusts it for backfill.
    pub(crate) max_lease_wall: Option<Duration>,
    /// The pool's `max_queue_wait` and `start_timeout`, which the tick's job pass enforces
    /// (their defaults for a pool that does not set them, and for `--agent-pool-config`).
    pub(crate) job_limits: JobLimits,
    /// The job store's setup when the job service is on; the agent side then opens it.
    pub(crate) jobs: Option<JobStoreSetup>,
    /// The clock and id source the job store is opened on: the system's, unless
    /// [`crate::Config::with_job_sources`] gave others.
    pub(crate) job_sources: JobSources,
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

/// `settings` with what the registered pool's file decides about whole-node leases: the
/// time a lease waits, pending, for its host to be ready is the pool's `prepare_timeout`
/// (the lease table counts it; the registry counts the same figure per commit). A pool
/// without whole-node settings, or no registered pool, leaves `settings` as the flags
/// made them.
pub(super) fn with_pool_settings(
    settings: Settings,
    hosts: &Hosts,
) -> Result<Settings, StartupError> {
    match hosts {
        Hosts::Layered { agents, .. } => {
            let whole = agents.pool.whole_node();
            let settings = with_whole_node(settings, whole)?;
            Ok(crate::reservations::with_restart_grace(settings, whole))
        }
        Hosts::Static(_) => Ok(settings),
    }
}

pub(super) fn with_whole_node(
    settings: Settings,
    whole: Option<&WholeNodeConfig>,
) -> Result<Settings, StartupError> {
    let Some(whole) = whole else {
        return Ok(settings);
    };
    settings
        .with_prepare_timeout(Duration::from_millis(whole.prepare_timeout_ms))
        .map_err(|err| StartupError::new("--agent-pool-config", err))
}
