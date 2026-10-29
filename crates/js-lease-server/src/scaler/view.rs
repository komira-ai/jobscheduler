//! The registry's words turned into the scaler's: a host as [`HostFacts`], a waiting
//! request as [`Waiting`].

use js_elastic::{GpuClass, Offer, PriceUnit};
use js_registry::{HostMark, HostState, HostView, Waiting as Request};
use js_scaler::{HostFacts, HostState as Hearing, Mark, Records, Waiting};
use js_types::{ExecMode, Shape};

/// The shape of a host in the form of an offer, so that one rule judges a host and an
/// offer alike. The registry knows no disk, price or availability of a host: the disk is
/// unbounded here (no request can name one yet), and the price and availability are not
/// read.
fn offer_of(shape: &Shape, exec: ExecMode) -> Offer {
    let resources = shape.resources();
    let capabilities = shape.capabilities();
    let text = |key: &str| capabilities.get(key).cloned().unwrap_or_default();
    Offer {
        id: String::new(),
        zone: None,
        cpu_millis: resources.cpu_millis(),
        memory_bytes: resources.memory_bytes(),
        disk_bytes: u64::MAX,
        gpus: u32::try_from(resources.gpus()).unwrap_or(u32::MAX),
        gpu_class: resources
            .gpu_class()
            .and_then(|class| GpuClass::parse(class).ok()),
        os: text("os"),
        arch: text("arch"),
        exec,
        price_cents_per_hour: None,
        price_unit: PriceUnit::UsCents,
        available: true,
    }
}

fn mark_of(mark: HostMark) -> Mark {
    match mark {
        HostMark::Reserved => Mark::Reserved,
        HostMark::Held => Mark::Held,
        HostMark::Draining => Mark::Draining,
        HostMark::Releasing => Mark::Releasing,
    }
}

fn hearing(state: HostState) -> Hearing {
    match state {
        HostState::Live => Hearing::Live,
        HostState::Lost => Hearing::Lost,
        HostState::Awaiting => Hearing::Awaiting,
    }
}

/// `host`, a host of the pool `pool` whose jobs run in `exec` mode, as the scaler reads it.
/// What it holds is what it announced when it did, and what the pool books for it
/// otherwise. It is busy when it holds a lease or still has runs to finish. It has given
/// up its leases when the registry does not hear from it and books no lease on it: a silent
/// host that never held one included.
///
/// A host that has announced nothing yet (one put back after a restart, not yet heard from)
/// has the shape its record stored, when `known` has one: the pool's floor, which is below
/// any offer, would otherwise condemn a healthy machine as short.
pub(super) fn host_facts(
    host: HostView,
    pool: &str,
    exec: ExecMode,
    known: impl Fn(&str) -> Option<Offer>,
) -> HostFacts {
    let announced = host.claim.as_ref().map(|claim| offer_of(claim, exec));
    let shape = match announced.or_else(|| known(host.id.as_str())) {
        Some(shape) => shape,
        None => offer_of(&host.shape, exec),
    };
    HostFacts {
        shape,
        pool: pool.to_owned(),
        state: hearing(host.state),
        mark: host.mark.map(mark_of),
        busy: host.lease.is_some() | host.releasing,
        lease_given_up: (host.state != HostState::Live) & host.lease.is_none(),
        id: host.id,
    }
}

/// The offer the record named `name` stored, if there is one.
pub(super) fn stored_shape(records: &Records, name: &str) -> Option<Offer> {
    let record = records
        .instances
        .iter()
        .find(|record| record.name == name)?;
    Some(record.offer.to_offer(record.price_cents_per_hour))
}

/// `request`, a waiting request, as the scaler reads it.
pub(super) fn waiting(request: Request) -> Waiting {
    Waiting {
        id: request.id,
        demand: request.demand,
        reserved: request.reserved,
    }
}
