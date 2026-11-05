//! Where a server's hosts come from: the hosts agents register (`--agent-pool-config`, or a
//! registering `[[pool]]` of `--config`), beside or instead of a simulated source a library
//! caller handed in through the simulation seam.

use std::time::Duration;

use js_lease::Settings;
use js_registry::{PoolConfig, WholeNodeConfig};

use super::{Cli, CpuSlack, Placement, Token};
use crate::agent_pool;
use crate::agent_tokens::AgentTokens;
use crate::capacity::CapacitySource;
use crate::error::StartupError;
use crate::rate::HelloLimit;

/// Where the hosts come from: the hosts agents register, or hosts a library caller handed
/// in through the simulation seam ([`Config::simulated`](crate::Config::simulated),
/// [`Config::with_capacity_source`](crate::Config::with_capacity_source),
/// or a kind of its own that builds [`Built::Simulated`](crate::Built::Simulated)). No flag
/// and no kind the server ships makes a simulated source.
pub(crate) enum Hosts {
    /// A simulated source alone, with no agent registration: tests and simulations only.
    Simulated(Box<dyn CapacitySource>),
    /// Agent registration is on.
    Layered {
        /// The simulated source, when a library caller handed one in; it is tried first.
        simulated: Option<Box<dyn CapacitySource>>,
        /// The registering side.
        agents: Box<AgentSetup>,
    },
}

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
        Hosts::Simulated(_) => Ok(settings),
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

/// The sources of hosts: the agent pool the flags name, beside the simulated source a
/// library caller handed in. One of the two is required, and the program hands in none;
/// the shared join token file needs the agent pool file, which alone turns registration
/// on.
pub(super) fn hosts(
    cli: &Cli,
    client: &Token,
    simulated: Option<Box<dyn CapacitySource>>,
) -> Result<Hosts, StartupError> {
    match (&cli.agent_token_file, &cli.agent_pool_config) {
        (None, None) => simulated.map(Hosts::Simulated).ok_or_else(|| {
            StartupError::new(
                "--agent-pool-config",
                "required: a server serves the hosts that run js-agent and register",
            )
        }),
        (tokens, Some(pool)) => {
            let tokens = match tokens {
                Some(path) => AgentTokens::open(path, client)?,
                None => AgentTokens::none(),
            };
            let pool = agent_pool::read(pool)?;
            cli.placement.check(&pool, "--placement")?;
            let needs = "--placement js-sched";
            let cpu_slack = cli.placement.slack(cli.cpu_slack, "--cpu-slack", needs)?;
            Ok(Hosts::Layered {
                simulated,
                agents: Box::new(AgentSetup {
                    tokens,
                    pool,
                    mint_provider: None,
                    hello_limit: HelloLimit {
                        burst: cli.agent_hello_burst,
                        interval: cli.agent_hello_interval,
                    },
                    require_host_credential: cli.require_host_credential,
                    placement: cli.placement,
                    estimate_feed: cli.estimate_feed,
                    cpu_slack,
                    max_lease_wall: None,
                }),
            })
        }
        (Some(_), None) => Err(StartupError::new(
            "--agent-pool-config",
            "required with --agent-token-file",
        )),
    }
}
