//! The `static` machine provider: the registry's host table behind one
//! [`js_elastic::CapacityProvider`].
//!
//! Its machines are the hosts that run `js-agent` and register themselves through
//! [`Registry::hello`]. It never launches one, so it has nothing to acquire, release or
//! put in a catalog, and what it answers about a machine it reads from the host table:
//!
//! - **Acquire** is refused with [`RefusalCode::Capacity`] for every request, and books,
//!   records and changes nothing: the caller falls through to its next provider.
//! - **Release** is refused as [`Claim::Foreign`] for every reference and leaves the host
//!   table as it is: this provider launched none of its machines, so it terminates none.
//!   A host leaves the table by the operator's [`Registry::retire`],
//!   [`Registry::release_host`] and [`Registry::remove`], or by the lost-host retention.
//! - **Observe** answers [`Observed::Present`] while the host is in the table and live,
//!   [`Observed::Unknown`] while it is in the table and silent (lost, or awaiting its
//!   first hello since a restart), and [`Observed::Absent`] only once the table no longer
//!   holds it: silence never frees (design 5.6). A host that holds a lease is never
//!   forgotten, so a booked host never observes `Absent`.
//!
//! It is a handle over the [`Registry`] it was made from: each call takes the registry's
//! one lock once, like every other registry call, reads the registry's own clock and
//! none of its own, and blocks on nothing else. It is not the lease-level
//! [`crate::RegisteredProvider`], which books leases on the same hosts.

use std::collections::BTreeSet;

use js_elastic::{
    AcquireRequest, Acquired, BootStep, Capacity, CapacityProvider, CatalogFuture, Claim, Held,
    Hello, HostRef, HostStatus, Observed, Offer, ProviderCaps, ProviderError, Reason, Refusal,
    RefusalCode, Released,
};
use js_types::{HostId, Model, ProviderId, normalise_arch};

use crate::registry::{HostMark, HostState, HostView, Registry};

/// The capability key under which a host reports its operating system.
const OS_KEY: &str = "os";

/// The capability key under which a host reports its CPU architecture.
const ARCH_KEY: &str = "arch";

/// The `static` [`CapacityProvider`]: the hosts of one [`Registry`], which register
/// themselves and are never launched. See the module documentation for what each call
/// answers.
pub struct StaticCapacity {
    registry: Registry,
}

impl StaticCapacity {
    /// The provider over the host table of `registry`. It holds a handle, so it sees every
    /// change made through any clone of the registry.
    pub fn new(registry: Registry) -> Self {
        Self { registry }
    }
}

/// A host as a machine of this provider: its host id is its reference and its name.
fn held(view: &HostView) -> Held {
    let id = view.id.as_str().to_owned();
    Held {
        id: HostRef(id.clone()),
        acquire: String::new(),
        name: Some(id),
        offer: String::new(),
        zone: None,
        status: status(view),
        ends_unix_seconds: None,
    }
}

/// The status of a host: being given back when it is marked releasing, else active when
/// it is live, and a status of its own (`lost`, `awaiting`) when it is silent.
fn status(view: &HostView) -> HostStatus {
    match (view.mark, view.state) {
        (Some(HostMark::Releasing), _) => HostStatus::Terminating,
        (_, HostState::Live) => HostStatus::Active,
        (_, HostState::Lost) => HostStatus::Unknown("lost".to_owned()),
        (_, HostState::Awaiting) => HostStatus::Unknown("awaiting".to_owned()),
    }
}

impl CapacityProvider for StaticCapacity {
    /// The registry's pool id.
    fn id(&self) -> ProviderId {
        self.registry.pool_id()
    }

    /// Always `false`: the hosts register themselves.
    fn can_launch(&self) -> bool {
        false
    }

    /// The operating systems, architectures (in the one spelling `normalise_arch` gives)
    /// and GPU classes the hosts in the table reported at their last hello. A host that
    /// has not said hello since a restart has reported nothing and adds nothing; a GPU
    /// class that is not in the agent's spelling is left out. Whole-host leases and host
    /// ports are offered when any host has reported, since every host runs `js-agent`.
    /// With no host that has reported, it is empty.
    fn capabilities(&self) -> ProviderCaps {
        let hosts = self.registry.hosts();
        let claims: Vec<_> = hosts
            .iter()
            .filter_map(|view| view.claim.as_ref())
            .collect();
        let reported = |key: &str| -> BTreeSet<String> {
            let values = claims
                .iter()
                .filter_map(|claim| claim.capabilities().get(key));
            values.cloned().collect()
        };
        let arch = reported(ARCH_KEY);
        let classes = claims
            .iter()
            .filter_map(|claim| claim.resources().gpu_class());
        let any = !claims.is_empty();
        ProviderCaps {
            os: reported(OS_KEY),
            arch: arch
                .iter()
                .map(|arch| normalise_arch(arch).to_owned())
                .collect(),
            accelerators: classes
                .filter_map(|class| Model::parse(class).ok())
                .map(|model| model.canonical().model)
                .collect(),
            whole_host: any,
            host_ports: any,
        }
    }

    /// `Ok` and empty: a static provider has nothing to launch. It is an answer, not an
    /// error. The future is already complete.
    fn catalog(&self) -> CatalogFuture<'_> {
        Box::pin(std::future::ready(Ok(Vec::new())))
    }

    /// The hosts in the table that are not being given back (live, lost, awaiting or
    /// draining), and `Some(0)` more: it can add none.
    fn capacity(&self) -> Capacity {
        let hosts = self.registry.hosts();
        let kept = hosts
            .iter()
            .filter(|view| view.mark != Some(HostMark::Releasing));
        Capacity {
            supplied: u32::try_from(kept.count()).unwrap_or(u32::MAX),
            can_add: Some(0),
        }
    }

    /// None: nothing is launched, so nothing is prepared.
    fn host_prep(&self, _offer: &Offer) -> Vec<BootStep> {
        Vec::new()
    }

    /// Zero: no payload is carried, since nothing boots.
    fn payload_limit(&self) -> usize {
        0
    }

    /// Refused with [`RefusalCode::Capacity`] and [`Reason::NotAllowed`] (it lists no
    /// offer), for every request and every repeat of one. Nothing is booked or recorded.
    fn acquire(&self, _request: &AcquireRequest) -> Acquired {
        Acquired::Refused(Refusal {
            code: RefusalCode::Capacity,
            reason: Reason::NotAllowed,
        })
    }

    /// Every host in the table, in id order, lost and awaiting ones included: the host id
    /// is the reference and the name, and there is no acquire id, offer or zone. The
    /// registry answers from memory, so this never fails.
    fn list(&self) -> Result<Vec<Held>, ProviderError> {
        Ok(self.registry.hosts().iter().map(held).collect())
    }

    /// Refused as [`Claim::Foreign`] for every reference, and the host table is left
    /// alone: this provider launched none of its machines.
    fn release(&self, _id: &HostRef) -> Released {
        Released::Refused(Claim::Foreign)
    }

    /// `Present` while the table holds the host and it is live; `Unknown` while it holds
    /// it silent, and for a reference that is not a host id; `Absent` once the table does
    /// not hold it.
    fn observe(&self, id: &HostRef) -> Observed {
        let Ok(host) = HostId::new(id.as_str()) else {
            return Observed::Unknown;
        };
        let hosts = self.registry.hosts();
        match hosts.iter().find(|view| view.id == host) {
            None => Observed::Absent,
            Some(view) if view.state == HostState::Live => Observed::Present(status(view)),
            Some(_) => Observed::Unknown,
        }
    }

    /// The host named `hello.name` when the instance its id is bound to ([`crate::Binding`],
    /// from the first hello that named one) has the id `hello.instance_id`; `None` for a
    /// hello with no instance id, a host bound to no instance or another one, and a name
    /// the table does not hold. It only reads: admission stays in [`Registry::hello`].
    fn verify_instance(&self, hello: &Hello) -> Option<HostRef> {
        let instance = hello.instance_id.as_deref()?;
        let host = HostId::new(&hello.name).ok()?;
        let bindings = self.registry.bindings();
        let bound = bindings.get(&host)?.instance()?;
        (bound.instance_id == instance).then(|| HostRef(host.as_str().to_owned()))
    }
}
