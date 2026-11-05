//! What the configuration decided for the agents that register hosts: [`AgentSetup`].

use std::time::Duration;

use js_registry::PoolConfig;

use super::{CpuSlack, Placement};
use crate::agent_tokens::AgentTokens;
use crate::job_expiry::JobLimits;
use crate::job_store::JobStoreSetup;
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
}
