//! The registry's words turned into the scaler's: a host as [`HostFacts`], a waiting
//! request as [`Waiting`].

use js_elastic::{GpuClass, Offer, PriceUnit};
use js_registry::{HostMark, HostState, HostView, Waiting as Request};
use js_scaler::{HostFacts, HostState as Hearing, Mark, Waiting};
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
/// What it holds is what it announced, if it did. It is busy when it holds a lease or still has runs to finish. It has given
/// up its leases when the registry does not hear from it and books no lease on it: a silent
/// host that never held one included.
///
/// `shape` is `Some` only when the host announced one (the registry's `claim`), and `None`
/// for a host that has announced none since the registry was put back after a restart. The
/// shell never stands in the pool's floor (`host.shape`) or a stored shape: the floor is
/// below any offer and would read as a hello that falls short.
pub(super) fn host_facts(host: HostView, pool: &str, exec: ExecMode) -> HostFacts {
    let shape = host.claim.as_ref().map(|claim| offer_of(claim, exec));
    HostFacts {
        shape,
        pool: pool.to_owned(),
        state: hearing(host.state),
        mark: host.mark.map(mark_of),
        busy: host.lease.is_some() | host.releasing,
        lease_given_up: (host.state != HostState::Live) & host.lease.is_none(),
        id: host.id,
        // The registry does not say whether the host has beaten since its hello, so the
        // scaler is told yes: the lost-Welcome rule stays off.
        heard_since_hello: true,
    }
}

/// `request`, a waiting request, as the scaler reads it.
pub(super) fn waiting(request: Request) -> Waiting {
    Waiting {
        id: request.id,
        demand: request.demand,
        reserved: request.reserved,
    }
}
