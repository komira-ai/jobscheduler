//! Admission of a submitted job (design 5.6, step 2): the views of the pool that
//! [`js_sched::admission_ever::can_ever_serve`] decides over, built from the registry's hosts
//! and the pool's launching machine providers, and the one decision a job submit takes from
//! it before anything is recorded.
//!
//! The hosts are read from the registry (taint, drain, cordon and silence). A launching
//! provider's catalog is read from the server's cache ([`Catalogs::view`]), which the tick
//! fills: the read calls no provider and answers at once. A catalog the cache has never
//! fetched is [`CatalogView::Unread`], which counts as "may offer".

use std::collections::BTreeMap;
use std::sync::Arc;

use js_agent_wire::HOST_JOBS_CAPABILITY;
use js_elastic::CapacityProvider;
use js_jobs::Isolation;
use js_registry::{HostMark, HostState as RegistryState, HostView as RegistryView, Registry};
use js_sched::admission_ever::{
    Admission, CatalogView, HostState, HostView, PoolLimits, ProviderView, can_ever_serve,
};
use js_types::{Requirements, Shape};

use crate::catalogs::{Cached, Catalogs};

/// A machine provider that launches for the pool, with what the pool lets it launch.
#[derive(Clone)]
pub(crate) struct Launching {
    /// The provider.
    pub(crate) provider: Arc<dyn CapacityProvider>,
    /// How many machines it may hold for the pool: its `provider_limits` `max_machines`, else
    /// the pool's.
    pub(crate) share: u32,
    /// The pool's `provider_limits` on it.
    pub(crate) limits: PoolLimits,
}

/// The pool's side of admission: its launching machine providers and the cache of their
/// catalogs. The hosts are read at each decision.
#[derive(Clone, Default)]
pub(crate) struct Admit {
    launching: Vec<Launching>,
    /// The cache of the launching providers' catalogs; `None` with none.
    cached: Option<Cached>,
}

impl Admit {
    /// Admission over `launching`, the pool's launching machine providers, whose catalogs
    /// are read from `cached`.
    pub(crate) fn new(launching: Vec<Launching>, cached: Cached) -> Self {
        Self {
            launching,
            cached: Some(cached),
        }
    }

    /// The views of the launching providers, each catalog as the cache holds it now.
    fn providers(&self) -> Vec<ProviderView> {
        self.cached
            .iter()
            .flat_map(|cached| {
                let now = cached.now();
                self.launching
                    .iter()
                    .map(move |one| provider_view(one, cached.catalogs(), now))
            })
            .collect()
    }
}

/// Admission of a submitted job (design section 5.6, step 2, `SubmitJob` step 3): the hosts
/// that register (a job runs on no other host), and the pool's launching machine providers.
/// A job with host isolation is offered only the registered hosts whose hello reported
/// `host-jobs`, and no provider: a machine not launched yet has reported nothing.
///
/// The hosts are read from the registry alone, whose lock the read takes and lets go at once. The launching providers' catalogs are read from the
/// server's cache, as a lease's are ([`Admit::decide`]): the read calls no provider and answers
/// at once. A catalog the cache has never fetched may offer.
pub(crate) struct JobAdmission {
    admit: Admit,
    registry: Registry,
}

impl JobAdmission {
    /// Admission over the hosts of `registry` and the launching providers of `admit`.
    pub(crate) fn new(admit: Admit, registry: Registry) -> Self {
        Self { admit, registry }
    }
}

#[tonic::async_trait]
impl crate::job_api::submit::Admission for JobAdmission {
    async fn can_ever_serve(&self, wanted: &Requirements, isolation: Isolation) -> bool {
        let on_the_machine = isolation == Isolation::Host;
        let hosts: Vec<HostView> = self
            .registry
            .hosts()
            .iter()
            .filter(|host| !on_the_machine | host.claim.as_ref().is_some_and(reports))
            .map(registered)
            .collect();
        let providers = match on_the_machine {
            true => Vec::new(),
            false => self.admit.providers(),
        };
        can_ever_serve(wanted, &hosts, &providers) != Admission::Refuse
    }
}

/// Whether a host's hello claim reports that its agent runs host jobs.
fn reports(claim: &Shape) -> bool {
    reports_host_jobs(claim.capabilities())
}

/// Whether `capabilities`, those a host's hello reported, say its agent runs host jobs
/// ([`HOST_JOBS_CAPABILITY`] is exactly `"true"`).
pub(crate) fn reports_host_jobs(capabilities: &BTreeMap<String, String>) -> bool {
    capabilities.get(HOST_JOBS_CAPABILITY).map(String::as_str) == Some("true")
}

/// `one`'s provider as admission sees it at `now`: enabled (no key turns a provider off),
/// with its share and limits, its capabilities, and its catalog as `catalogs` holds it. A
/// provider that launches nothing (`static`) has no catalog, and none is read.
pub(crate) fn provider_view(
    one: &Launching,
    catalogs: &Catalogs,
    now: js_elastic::Instant,
) -> ProviderView {
    let provider = one.provider.as_ref();
    let launches = provider.can_launch();
    let catalog = match launches {
        true => catalogs.view(&provider.id(), now),
        false => CatalogView::Offers(Vec::new()),
    };
    ProviderView {
        kind_is_static: !launches,
        enabled: true,
        share: one.share,
        capabilities: provider.capabilities(),
        catalog,
        limits: one.limits.clone(),
    }
}

/// A host of the registry as admission sees it, on the shape the registry places it on
/// ([`RegistryView::placement_shape`]: the pool's booking with the host's validated
/// accelerator facts). It is tainted when it reported a taint or
/// announces the tainted phase; draining when it is being released ([`HostMark::Releasing`]:
/// it is leaving the pool, and nothing ends the mark); lost when it is not live (silent
/// past its grace, or not heard since a restart). A drain mark ([`HostMark::Draining`]) is
/// not leaving: [`Registry::undrain`] ends it, as the scaler does for a request that needs
/// the host. Nor is the phase an agent announces when it is told to stop (`draining`): the
/// agent comes back, as after a restart. Such a host stays capable.
pub(crate) fn registered(host: &RegistryView) -> HostView {
    let placed = host.placement_shape();
    HostView {
        id: host.id.clone(),
        state: state_of(
            host.state != RegistryState::Live,
            host.cordon_until.is_some(),
            !host.leases.is_empty(),
        ),
        tainted: host.taint.is_some() | (host.phase == js_agent_wire::Phase::Tainted),
        draining: host.mark == Some(HostMark::Releasing),
        total: placed.resources().clone(),
        facts: placed.capabilities().clone(),
    }
}

/// The state of a host that is `lost`, `cordoned` and `busy`, in that order of precedence.
fn state_of(lost: bool, cordoned: bool, busy: bool) -> HostState {
    match (lost, cordoned, busy) {
        (true, _, _) => HostState::Lost,
        (false, true, _) => HostState::Cordoned,
        (false, false, true) => HostState::Busy,
        (false, false, false) => HostState::Serving,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts the precedence of a host's state: silence first, then a cordon, then work.
    #[test]
    fn a_host_state_is_lost_then_cordoned_then_busy() {
        assert_eq!(state_of(true, true, true), HostState::Lost);
        assert_eq!(state_of(false, true, true), HostState::Cordoned);
        assert_eq!(state_of(false, false, true), HostState::Busy);
        assert_eq!(state_of(false, false, false), HostState::Serving);
    }

    /// Asserts that a hello reports host jobs only with the capability `host-jobs` set to
    /// exactly `true`: absent, `false`, another case or a padded value do not. Catches a
    /// host that reports nothing, or something else, offered a host job.
    #[test]
    fn host_jobs_are_reported_only_by_the_value_true() {
        let said = |value: Option<&str>| {
            let caps: BTreeMap<String, String> = value
                .map(|value| (HOST_JOBS_CAPABILITY.to_owned(), value.to_owned()))
                .into_iter()
                .collect();
            reports_host_jobs(&caps)
        };
        assert!(said(Some("true")));
        for other in [None, Some("false"), Some("True"), Some("true ")] {
            assert!(!said(other), "{other:?}");
        }
    }
}
