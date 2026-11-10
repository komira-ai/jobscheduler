//! Which machine provider of the Default platform a registered host is of, and where the
//! platform lists that provider (design section 5.6, steps 3 and 4): the rank the job pass
//! gives each open host ([`js_sched_placer::engine::jobs::OpenHost::rank`]).
//!
//! The order is the Default platform's `machine_providers`, first to last
//! ([`order_of`]); a configuration that names no platform keeps the order the server always
//! had, `static` first and then the pool's launched hosts ([`static_then`]). A host's provider
//! comes from its binding alone ([`rank`]): a host whose binding names no instance (it joined
//! with the pool's join token) is of the `static` provider, and one whose binding names an
//! instance (it joined with a token minted for that instance) is of the pool's launching
//! providers. A binding does not say which launching provider launched its machine, so a
//! pool that launches with several ranks every launched host at the place of the first of
//! them the platform lists; the engine's score decides among them. Nothing here reads a host's
//! name, its address, the provider text its instance carries or a catalog. Pure: no I/O, no
//! clock.

use js_registry::Binding;
use js_sched::ProviderRank;
use js_types::ProviderId;

use crate::file::Resolved;

/// The `static` machine provider: the hosts that join with the pool's join token and whose
/// binding names no instance. Every other provider of an order is the pool's launching one.
pub(crate) const STATIC: ProviderId = ProviderId::known("static");

/// The order of a configuration that names no platform: `static`, then the hosts launched for
/// the pool `pool` (those whose bindings name an instance).
pub(crate) fn static_then(pool: &ProviderId) -> Vec<ProviderId> {
    vec![STATIC, pool.clone()]
}

/// The machine providers of the pool `pool`, first to last: those of each of its platforms
/// whose jobs run on hosts of ours, in the order the pool lists its platforms and each
/// platform its `machine_providers`, each once, as the provider a binding names: a `static`
/// machine provider as [`STATIC`], each one that launches as `pool`, the provider the scaler
/// mints the pool's machines under, so that several launching providers are one entry at the
/// place of the first. `None` when `resolved` holds no such platform for the
/// pool: the order is then [`static_then`].
pub(crate) fn order_of(resolved: &Resolved, pool: &ProviderId) -> Option<Vec<ProviderId>> {
    let named = resolved.pool(pool.as_str())?;
    let platforms = named.platforms.iter().filter_map(|id| {
        let mut declared = resolved.platforms.iter();
        let platform = declared.find(|platform| platform.id == *id);
        platform.filter(|platform| platform.kind.has_hosts())
    });
    let names = platforms.flat_map(|platform| platform.machine_providers.iter());
    let providers = names.filter_map(|name| {
        let mut declared = resolved.machine_providers.iter();
        declared.find(|provider| provider.name == *name)
    });
    let mut order: Vec<ProviderId> = Vec::new();
    for provider in providers.map(|provider| match provider.launches() {
        true => pool.clone(),
        false => STATIC,
    }) {
        if !order.contains(&provider) {
            order.push(provider);
        }
    }
    (!order.is_empty()).then_some(order)
}

/// The rank of a host whose binding is `binding` in the platform whose machine providers are
/// `order`, first to last, each as [`order_of`] names it: the position of the `static`
/// provider for a host whose binding names no instance (or that has none), the position of the
/// launching provider (the first that is not `static`) for one whose binding names an
/// instance. `None` when the platform lists no provider of that kind: such a host is no
/// candidate on this platform (it stays in the host table).
pub(crate) fn rank(order: &[ProviderId], binding: Option<&Binding>) -> Option<ProviderRank> {
    let launched = binding.and_then(Binding::instance).is_some();
    let mut listed = order.iter().enumerate();
    let (position, provider) = listed.find(|(_, provider)| (**provider != STATIC) == launched)?;
    let position = u32::try_from(position).unwrap_or(u32::MAX);
    Some(ProviderRank::new(provider.clone(), position))
}

#[cfg(test)]
#[path = "rank_tests.rs"]
mod tests;
