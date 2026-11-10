//! One launch: the boot plan of a machine, rendered with its secrets and handed to the
//! provider the scaler picked for it among the pool's. This is the only place that holds
//! both a network mode and a provider; neither knows the other. The provider receives the
//! payload and nothing else.
//!
//! It is also the one place the server receives a provider's answer to an acquire, so it
//! hands each answer to the cache of the provider's catalog ([`Catalogs::acquired`]): a
//! refusal for capacity or quota says the cached offers are out of date.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use js_elastic::{
    AcquireRequest, Acquired, Artifact, BootPlan, CapacityProvider, Cooldowns, HostName, Instant,
    Offer, Reason, Refusal, RefusalCode, SchedulerId, Secret,
};
use js_scaler::AcquirePlan;
use js_types::{ProviderId, normalise_arch};
use tracing::warn;

use crate::bootstrap::{BootSecrets, payload};
use crate::catalogs::Catalogs;
use crate::network::{LaunchCtx, NetworkError, NetworkMode};

/// How long the network mode's secret for one launch (a Tailscale key) stays valid.
pub const KEY_VALID_FOR: Duration = Duration::from_secs(10 * 60);

/// How long a launch whose secret could not be made waits before the scaler tries again.
pub const RETRY_AFTER: Duration = Duration::from_secs(30);

/// What a launch needs besides the scaler's decision: the pool's providers, its network
/// mode and what the machine is told to dial and run.
pub struct Launcher {
    /// The pool's providers, first to last; never empty.
    providers: Vec<Arc<dyn CapacityProvider>>,
    mode: Arc<dyn NetworkMode>,
    scheduler: SchedulerId,
    scheduler_url: String,
    agents: BTreeMap<String, Artifact>,
    catalogs: Arc<Catalogs>,
}

impl Launcher {
    /// A launcher for the machines of `scheduler`: each is told to download the entry of `agents` for its offer's architecture, to
    /// dial `scheduler_url`, and to join the network as `mode` says. Its provider's answers
    /// go to no catalog cache until [`Launcher::with_catalogs`] gives it one.
    pub fn new(
        provider: Arc<dyn CapacityProvider>,
        mode: Arc<dyn NetworkMode>,
        scheduler: SchedulerId,
        scheduler_url: String,
        agents: BTreeMap<String, Artifact>,
    ) -> Self {
        Self {
            providers: vec![provider],
            mode,
            scheduler,
            scheduler_url,
            agents,
            catalogs: Arc::new(Catalogs::new()),
        }
    }

    /// This launcher, also launching with `provider`, after the providers it has: a launch
    /// goes to the provider its plan names ([`AcquirePlan::provider`]).
    #[must_use]
    pub fn with_provider(mut self, provider: Arc<dyn CapacityProvider>) -> Self {
        self.providers.push(provider);
        self
    }

    /// This launcher, handing each answer its provider gives to an acquire to `catalogs`
    /// ([`Catalogs::acquired`]), which invalidates the provider's cached catalog on a
    /// refusal for capacity or quota.
    #[must_use]
    pub fn with_catalogs(mut self, catalogs: Arc<Catalogs>) -> Self {
        self.catalogs = catalogs;
        self
    }

    /// Takes the offer cool-downs `cooled` into its catalogs ([`Catalogs::hold`]).
    pub(crate) fn hold(&self, cooled: &Cooldowns) {
        self.catalogs.hold(cooled);
    }

    /// How many fetches of its providers' catalogs completed ([`Catalogs::fetches`]),
    /// every provider's together.
    pub(crate) fn catalog_fetches(&self) -> u64 {
        let fetches = self
            .providers
            .iter()
            .map(|each| self.catalogs.fetches(&each.id()));
        fetches.fold(0, u64::saturating_add)
    }

    /// The offers each of its providers after the first may launch at `now`, by provider:
    /// its cached catalog less the offers cooling down ([`Catalogs::offers_for_pass2`]).
    pub(crate) fn later_offers(&self, now: Instant) -> BTreeMap<ProviderId, Vec<Offer>> {
        let later = self.providers.iter().skip(1).map(|each| {
            let id = each.id();
            let offers = self.catalogs.offers_for_pass2(&id, now);
            (id, offers)
        });
        later.collect()
    }

    /// The provider `plan` names, or the first when it names none of them: a scaler names
    /// only providers of its pool.
    fn provider(&self, plan: &AcquirePlan) -> &Arc<dyn CapacityProvider> {
        let named = self
            .providers
            .iter()
            .find(|each| each.id() == plan.provider);
        named.unwrap_or(&self.providers[0])
    }

    /// Launches the machine `plan` describes, booting with the join token `token`.
    ///
    /// The network mode's secret is made now, the boot plan is rendered and the payload is
    /// handed to the provider as it is. If the mode cannot make its secret nothing is
    /// sent, and the answer is [`Acquired::Deferred`]: the scaler tries again later, minting
    /// a new token. A plan that cannot be rendered (a payload over the provider's limit, a
    /// slot twice) is a [`RefusalCode::Shape`] refusal, which nothing retries.
    pub fn acquire(&self, plan: &AcquirePlan, token: Secret) -> Acquired {
        let provider = self.provider(plan);
        let host = HostName::derive(&self.scheduler, &plan.acquire);
        // The pins are keyed by the architecture in the one spelling, an offer's own.
        let arch = normalise_arch(&plan.offer.arch);
        let Some(agent) = self.agents.get(arch) else {
            return Self::unpinned(arch);
        };
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
            agent: agent.clone(),
            scheduler_url: self.scheduler_url.clone(),
            pool: plan.pool.clone(),
            host: host.clone(),
            host_prep: provider.host_prep(&plan.offer),
            network: contribution.steps,
        };
        let limit = provider.payload_limit();
        let rendered = BootSecrets::new(token, contribution.secrets)
            .and_then(|secrets| payload(&boot, &secrets, limit));
        match rendered {
            Ok(payload) => {
                let answer = provider.acquire(&AcquireRequest {
                    acquire: plan.acquire.clone(),
                    pool: plan.pool.clone(),
                    name: host,
                    offer: plan.offer.clone(),
                    lifetime: plan.lifetime,
                    labels: Vec::new(),
                    payload,
                });
                self.catalogs.acquired(&provider.id(), &answer);
                answer
            }
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

    /// The refusal of a launch of an architecture the pool has no agent for: never another
    /// architecture's binary. Nothing retries it.
    fn unpinned(arch: &str) -> Acquired {
        Acquired::Refused(Refusal {
            code: RefusalCode::Shape,
            reason: Reason::Boot(format!("no agent is pinned for the architecture {arch:?}")),
        })
    }

    fn deferred(&self, pool: &str, err: &NetworkError) -> Acquired {
        warn!(pool, %err, "cannot make the network secret of a launch; it is tried again");
        Acquired::Deferred { wait: RETRY_AFTER }
    }
}
