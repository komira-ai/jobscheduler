//! Admission of a lease request (design 5.6, step 2): the views of the pool that
//! [`js_sched::admission_ever::can_ever_serve`] decides over, built from the host table and
//! the pool's launching machine provider, and the one decision `POST /v1/leases` takes from
//! it before anything is recorded.
//!
//! The hosts are read under the table lock, from the provider the table runs on (every
//! host of the static pool and of the registry) and from the registry (taint, drain, cordon
//! and silence). A catalog is a call that may take long, so it is never read with the lock
//! held: [`Admit::decide`] answers [`Decision::ReadCatalogs`] when the hosts alone admit
//! nothing and the pool has a provider that launches, the route lets the lock go, reads
//! the catalogs ([`Admit::read`]) on the pool for blocking calls, and asks again under the
//! lock with them.

use std::collections::BTreeSet;
use std::sync::Arc;

use js_elastic::CapacityProvider;
use js_provider::{HostCapacity, Pool, StartSpec};
use js_registry::{HostMark, HostState as RegistryState, HostView as RegistryView, Registry};
use js_sched::admission_ever::{
    Admission, CatalogView, HostState, HostView, PoolLimits, ProviderView, can_ever_serve,
};
use js_types::{HostId, LeaseId, Request, RequestId, Requirements};

use crate::api::Table;
use crate::util::blocking;

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

/// The pool's side of admission: its launching machine providers. The hosts are read at
/// each decision.
#[derive(Clone, Default)]
pub(crate) struct Admit {
    launching: Vec<Launching>,
}

/// What the route does with a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decision {
    /// Acquire it, as before admission: something could serve it, or it repeats a request
    /// id the table holds a lease for.
    Start,
    /// Refuse it with `no-capable-host`, recording nothing.
    Refuse,
    /// No host could serve it: read the catalogs, with the table lock let go, and ask again.
    ReadCatalogs,
}

impl Admit {
    /// Admission over `launching`, the pool's launching machine providers.
    pub(crate) fn new(launching: Vec<Launching>) -> Self {
        Self { launching }
    }

    /// What to do with `request`, under `request_id`, given the table (held by the caller),
    /// the registry when agents register hosts, and `providers`: the catalogs once read
    /// ([`Self::read`]), `None` before.
    ///
    /// The table answers as before, and admission is not asked, for a request whose id
    /// names a lease the table holds (a repeat: admission never refuses a request it already
    /// took) and for one that breaks the start contract (its class, labels or wall time),
    /// which keeps its own refusal. Otherwise [`can_ever_serve`] decides; before the
    /// catalogs are read only the hosts are asked, and a refusal of a pool with a launching
    /// provider asks for them.
    pub(crate) fn decide(
        &self,
        table: &Table,
        registry: Option<&Registry>,
        request_id: Option<&RequestId>,
        request: &Request,
        providers: Option<&[ProviderView]>,
    ) -> Decision {
        let known = table.request_ids();
        let repeat = request_id.is_some_and(|id| known.iter().any(|(_, held)| held == id));
        let classes = table.provider().classes();
        let contract = StartSpec::new(LeaseId::new(0, 0), request.clone(), &classes).is_ok();
        let hosts = host_views(table.provider().capacity(), registry);
        let wanted = Requirements::from_request(request);
        let verdict = can_ever_serve(&wanted, &hosts, providers.unwrap_or_default());
        let asked = providers.is_some() | self.launching.is_empty();
        match (repeat | !contract, verdict, asked) {
            (true, _, _) | (false, Admission::Accept(_), _) => Decision::Start,
            (false, Admission::Refuse, true) => Decision::Refuse,
            (false, Admission::Refuse, false) => Decision::ReadCatalogs,
        }
    }

    /// The views of the launching providers, each catalog read once, on the pool for
    /// blocking calls.
    pub(crate) async fn read(&self) -> Vec<ProviderView> {
        let launching = self.launching.clone();
        blocking(move || {
            launching
                .iter()
                .map(|one| provider_view(one.provider.as_ref(), one.share, one.limits.clone()))
                .collect()
        })
        .await
    }
}

/// Admission of a submitted job (design section 5.6, step 2, `SubmitJob` step 3): the hosts
/// that register (a job runs on no other host), and the pool's launching machine providers.
///
/// The hosts are read first, from the registry alone, whose lock each read takes and lets go
/// at once; the lease table's lock is never taken. Only when no host could serve the job, and
/// a provider launches, are the catalogs read ([`Admit::read`], on the pool for blocking
/// calls), with no lock held. A catalog that cannot be read may offer.
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
    async fn can_ever_serve(&self, wanted: &Requirements) -> bool {
        let hosts: Vec<HostView> = self.registry.hosts().iter().map(registered).collect();
        let by_hosts = can_ever_serve(wanted, &hosts, &[]) != Admission::Refuse;
        if by_hosts | self.admit.launching.is_empty() {
            return by_hosts;
        }
        let providers = self.admit.read().await;
        can_ever_serve(wanted, &hosts, &providers) != Admission::Refuse
    }
}

/// `provider` as admission sees it: enabled (no key turns a provider off), with `share` and
/// `limits`, its capabilities, and its catalog. A provider that launches nothing (`static`)
/// has no catalog, and none is read.
pub(crate) fn provider_view(
    provider: &dyn CapacityProvider,
    share: u32,
    limits: PoolLimits,
) -> ProviderView {
    let launches = provider.can_launch();
    let catalog = match launches {
        // TODO(js-catalog-cache): read the provider's cached catalog instead of calling it.
        true => provider
            .catalog()
            .map_or(CatalogView::Unread, CatalogView::Offers),
        false => CatalogView::Offers(Vec::new()),
    };
    ProviderView {
        kind_is_static: !launches,
        enabled: true,
        share,
        capabilities: provider.capabilities(),
        catalog,
        limits,
    }
}

/// The hosts of `listed` (every host the table's provider lists, with the shape it books)
/// as admission sees them: a host the registry knows as `registry` reads it, any other (a
/// host of the static pool) is serving, or busy while it holds a lease.
pub(crate) fn host_views(listed: Vec<HostCapacity>, registry: Option<&Registry>) -> Vec<HostView> {
    let registered: Vec<HostView> = registry
        .map(Registry::hosts)
        .unwrap_or_default()
        .iter()
        .map(registered)
        .collect();
    let known: BTreeSet<HostId> = registered.iter().map(|host| host.id.clone()).collect();
    let fixed = listed
        .into_iter()
        .filter(|host| !known.contains(&host.host))
        .map(|host| HostView {
            state: state_of(false, false, !host.leases.is_empty()),
            id: host.host,
            tainted: false,
            draining: false,
            total: host.shape.resources().clone(),
            facts: host.shape.capabilities().clone(),
        });
    registered.into_iter().chain(fixed).collect()
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
}
