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
use js_types::{Capabilities, DISK_GIB_KEY, EXEC_KEY, Requirements};

/// One GiB, the unit of the `disk-gib` capability.
const GIB: u64 = 1 << 30;

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

    /// Whether a machine provider that launches says it could offer a machine for `r`
    /// ([`PlatformCaps::admits`]), or a registered host could ever hold it: the registry's
    /// own check before it refuses a whole-node start for want of a capable host
    /// ([`Registry::could_ever_hold`]). A provider that launches nothing is judged by its
    /// hosts one by one, not by its capabilities, which pool every host's facts and say
    /// nothing of size. No catalog is read: that call is slow and is cached later.
    ///
    /// This check is interim: admission's own `can_ever_serve` (design 5.6, step 2)
    /// replaces its body.
    fn can_ever_serve(&self, r: &Requirements) -> bool {
        let offered = self
            .providers
            .iter()
            .filter(|provider| provider.can_launch())
            .any(|provider| caps_of(&provider.capabilities()).admits(r));
        let held = self
            .hosts
            .as_ref()
            .is_some_and(|hosts| hosts.could_ever_hold(&r.resources, &required(r)));
        offered | held
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

/// The capabilities a host must report for `r`, as a lease request names them: `os`,
/// `arch`, `disk-gib` (rounded up to a whole GiB), `isa_level` and `exec`, each only when
/// `r` names it.
fn required(r: &Requirements) -> Capabilities {
    let named = [
        ("os", r.os.clone()),
        ("arch", r.arch.clone()),
        (
            DISK_GIB_KEY,
            r.disk_bytes.map(|bytes| bytes.div_ceil(GIB).to_string()),
        ),
        ("isa_level", r.isa_level.map(|level| level.to_string())),
        (EXEC_KEY, r.exec.map(|mode| mode.name().to_owned())),
    ];
    named
        .into_iter()
        .filter_map(|(key, value)| value.map(|value| (key.to_owned(), value)))
        .collect()
}
