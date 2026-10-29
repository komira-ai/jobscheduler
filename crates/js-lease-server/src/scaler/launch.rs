//! One launch: the boot plan of a machine, rendered with its secrets and handed to the
//! provider. This is the only place that holds both a network mode and a provider; neither
//! knows the other. The provider receives the payload and nothing else.

use std::sync::Arc;
use std::time::Duration;

use js_elastic::{
    AcquireRequest, Acquired, Artifact, BootPlan, ElasticProvider, HostName, Reason, Refusal,
    RefusalCode, SchedulerId, Secret,
};
use js_scaler::AcquirePlan;
use tracing::warn;

use crate::bootstrap::{BootSecrets, payload};
use crate::network::{LaunchCtx, NetworkError, NetworkMode};

/// How long the network mode's secret for one launch (a Tailscale key) stays valid.
pub const KEY_VALID_FOR: Duration = Duration::from_secs(10 * 60);

/// How long a launch whose secret could not be made waits before the scaler tries again.
pub const RETRY_AFTER: Duration = Duration::from_secs(30);

/// What a launch needs besides the scaler's decision: the provider, the pool's network
/// mode and what the machine is told to dial and run.
pub struct Launcher {
    provider: Arc<dyn ElasticProvider>,
    mode: Arc<dyn NetworkMode>,
    scheduler: SchedulerId,
    scheduler_url: String,
    agent: Artifact,
}

impl Launcher {
    /// A launcher for the machines of `scheduler`: each is told to download `agent`, to
    /// dial `scheduler_url`, and to join the network as `mode` says.
    pub fn new(
        provider: Arc<dyn ElasticProvider>,
        mode: Arc<dyn NetworkMode>,
        scheduler: SchedulerId,
        scheduler_url: String,
        agent: Artifact,
    ) -> Self {
        Self {
            provider,
            mode,
            scheduler,
            scheduler_url,
            agent,
        }
    }

    /// Launches the machine `plan` describes, booting with the join token `token`.
    ///
    /// The network mode's secret is made now, the boot plan is rendered and the payload is
    /// handed to the provider as it is. If the mode cannot make its secret nothing is
    /// sent, and the answer is [`Acquired::Deferred`]: the scaler tries again later, minting
    /// a new token. A plan that cannot be rendered (a payload over the provider's limit, a
    /// slot twice) is a [`RefusalCode::Shape`] refusal, which nothing retries.
    pub fn acquire(&self, plan: &AcquirePlan, token: Secret) -> Acquired {
        let host = HostName::derive(&self.scheduler, &plan.acquire);
        let launch = LaunchCtx {
            pool: &plan.pool,
            host: &host,
            valid_for: KEY_VALID_FOR,
        };
        let contribution = match self.mode.prepare(&launch) {
            Ok(contribution) => contribution,
            Err(err) => return self.deferred(&plan.pool, &err),
        };
        let boot = BootPlan {
            agent: self.agent.clone(),
            scheduler_url: self.scheduler_url.clone(),
            pool: plan.pool.clone(),
            host: host.clone(),
            host_prep: self.provider.host_prep(&plan.offer),
            network: contribution.steps,
        };
        let limit = self.provider.payload_limit();
        let rendered = BootSecrets::new(token, contribution.secrets)
            .and_then(|secrets| payload(&boot, &secrets, limit));
        match rendered {
            Ok(payload) => self.provider.acquire(&AcquireRequest {
                acquire: plan.acquire.clone(),
                pool: plan.pool.clone(),
                name: host,
                offer: plan.offer.clone(),
                labels: Vec::new(),
                payload,
            }),
            Err(err) => Acquired::Refused(Refusal {
                code: RefusalCode::Shape,
                reason: Reason::Boot(err.to_string()),
            }),
        }
    }

    /// Cleans up the network side of a released machine, if the mode has any. Best effort:
    /// a failure is logged and the release goes on.
    pub fn forget(&self, host: &HostName) {
        if let Err(err) = self.mode.forget(host) {
            warn!(%host, %err, "cannot forget the network device of a released host");
        }
    }

    fn deferred(&self, pool: &str, err: &NetworkError) -> Acquired {
        warn!(pool, %err, "cannot make the network secret of a launch; it is tried again");
        Acquired::Deferred { wait: RETRY_AFTER }
    }
}
