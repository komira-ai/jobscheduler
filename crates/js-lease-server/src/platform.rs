//! The `default` container platform: hosts of ours, supplied by an ordered list of machine
//! providers (design 3.5).
//!
//! For `default` the job engine books each job's allocation on a host, so the platform
//! answers only the three required calls of [`ContainerPlatform`]; the five calls of a job's
//! life keep their default answer. The server still runs the fixed order of `layered.rs`,
//! registered hosts and then the elastic provider; this type is what that order becomes.

use std::sync::Arc;

use js_elastic::{CapacityProvider, ProviderCaps};
use js_platform::{ContainerPlatform, PlatformCaps, PlatformId};
use js_registry::Registry;
use js_sched::admission_ever::{Admission, PoolLimits, can_ever_serve};
use js_types::Requirements;

use crate::admit;

/// The `default` platform: its machine providers, first to last, and the host table of its
/// `static` provider, whose hosts register themselves.
pub struct DefaultPlatform {
    id: PlatformId,
    providers: Vec<Arc<dyn CapacityProvider>>,
    hosts: Option<Registry>,
}

impl DefaultPlatform {
    /// The platform `id` over `providers`, in the order its resolved platform names its
    /// machine providers, and `hosts`, the host table of its `static` machine provider when
    /// it names one.
    pub fn new(
        id: PlatformId,
        providers: Vec<Arc<dyn CapacityProvider>>,
        hosts: Option<Registry>,
    ) -> Self {
        Self {
            id,
            providers,
            hosts,
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
    /// catalog read once (one that cannot be read may offer). The platform is built without
    /// a pool, so it applies no pool limit and bounds no provider's share.
    fn can_ever_serve(&self, r: &Requirements) -> bool {
        let hosts = self.hosts.as_ref().map(Registry::hosts).unwrap_or_default();
        let hosts: Vec<_> = hosts.iter().map(admit::registered).collect();
        let providers: Vec<_> = self
            .providers
            .iter()
            .map(|provider| {
                admit::provider_view(provider.as_ref(), u32::MAX, PoolLimits::default())
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
