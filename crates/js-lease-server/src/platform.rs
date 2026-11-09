//! The `default` container platform: hosts of ours, supplied by an ordered list of machine
//! providers (design 3.5).
//!
//! For `default` the job engine books each job's allocation on a host, so the platform
//! answers only the three required calls of [`ContainerPlatform`]; the five calls of a job's
//! life keep their default answer. The server still runs a fixed order,
//! registered hosts and then the elastic provider; this type is what that order becomes.

use std::sync::Arc;

use js_elastic::{CapacityProvider, ProviderCaps};
use js_platform::{ContainerPlatform, PlatformCaps, PlatformId};
use js_registry::Registry;
use js_sched::admission_ever::{Admission, PoolLimits, can_ever_serve};
use js_types::Requirements;

use crate::admit::{self, Launching};
use crate::catalogs::{Cached, Catalogs};
use crate::clock::SharedClock;

/// The `default` platform: its machine providers, first to last, the host table of its
/// `static` provider, whose hosts register themselves, and the cache of its launching
/// providers' catalogs.
pub struct DefaultPlatform {
    id: PlatformId,
    providers: Vec<Arc<dyn CapacityProvider>>,
    hosts: Option<Registry>,
    cached: Cached,
}

impl DefaultPlatform {
    /// The platform `id` over `providers`, in the order its resolved platform names its
    /// machine providers, and `hosts`, the host table of its `static` machine provider when
    /// it names one. Its launching providers' catalogs are read from `catalogs` at the time
    /// of `clock`; a provider `catalogs` has not fetched may offer.
    pub fn new(
        id: PlatformId,
        providers: Vec<Arc<dyn CapacityProvider>>,
        hosts: Option<Registry>,
        catalogs: Arc<Catalogs>,
        clock: SharedClock,
    ) -> Self {
        Self {
            id,
            providers,
            hosts,
            cached: Cached::new(catalogs, clock),
        }
    }
}

impl ContainerPlatform for DefaultPlatform {
    fn id(&self) -> PlatformId {
        self.id.clone()
    }

    /// The union of what its machine providers say they could ever offer.
    fn capabilities(&self) -> PlatformCaps {
        self.providers
            .iter()
            .map(|provider| caps_of(&provider.capabilities()))
            .fold(PlatformCaps::default(), |all, one| all.union(&one))
    }

    /// Whether anything of the platform could ever serve `r`: admission's own decision
    /// ([`js_sched::admission_ever::can_ever_serve`], design 5.6, step 2) over the hosts of
    /// its host table, as the lease route reads them, and its machine providers, each
    /// catalog as the cache holds it (one never fetched may offer); no provider is called.
    /// The platform is built without a pool, so it applies no pool limit and bounds no
    /// provider's share.
    fn can_ever_serve(&self, r: &Requirements) -> bool {
        let hosts = self.hosts.as_ref().map(Registry::hosts).unwrap_or_default();
        let hosts: Vec<_> = hosts.iter().map(admit::registered).collect();
        let now = self.cached.now();
        let providers: Vec<_> = self
            .providers
            .iter()
            .map(|provider| {
                let one = Launching {
                    provider: Arc::clone(provider),
                    share: u32::MAX,
                    limits: PoolLimits::default(),
                };
                admit::provider_view(&one, self.cached.catalogs(), now)
            })
            .collect();
        can_ever_serve(r, &hosts, &providers) != Admission::Refuse
    }
}

/// A provider's capabilities in the platform's terms: an operating system or architecture
/// that is not one of the vocabulary's is left out.
fn caps_of(caps: &ProviderCaps) -> PlatformCaps {
    PlatformCaps::new(
        caps.os.iter().filter_map(|os| os.parse().ok()).collect(),
        caps.arch
            .iter()
            .filter_map(|arch| arch.parse().ok())
            .collect(),
        caps.accelerators.iter().map(ToString::to_string).collect(),
        caps.whole_host,
    )
}
