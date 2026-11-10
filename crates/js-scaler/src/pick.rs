//! Pass 2 of placement for one pool (design 5.6, step 5). The scaler does not choose the
//! provider or the offer a launch takes: it gives [`js_sched::acquire::pick`] the pool's
//! machine providers in their order and stages the offer of the provider the answer names,
//! or shows why there is none.
//!
//! The input holds one [`ProviderView`] per provider of the pool, first to last
//! ([`Settings::providers`]): its offers (the first provider's are
//! [`PoolInput::offers`], each later one's are in [`PoolInput::later_offers`]; [`walk`]
//! leaves out what the request's refusals rule out), less those outside its
//! `provider_limits` (`allowed_instance_types`, `max_price_cents_per_hour`, which each
//! provider has its own of), its share, and the machines the pool holds of it. A record
//! that names no provider is the first's. Each view is enabled and neither cooling down nor
//! braked. The scaler keeps the pool's cooldown and its brake and applies them itself: a
//! cooling pool launches nothing before the answer is read, and a tripped pool still shows
//! what it would launch. Passing them in too would hide those lines, and turn the
//! `launchable` answer for a cooling pool from yes to no. The pool's total is
//! `max_instances`, which the caps hold, so the input sets none. No configuration key gives
//! a provider a `purchase` order yet, so a view has none and that tie of the pick does not
//! separate two offers.

use js_elastic::{Demand, Instant, Offer};
use js_sched::acquire::{
    AcquireDecision, AcquireInput, ProviderView, Skip, SkipReason, Waiting, pick,
};
use js_sched::failover::{AcquireMemory, Next};
use js_types::{ProviderId, RequestId};

use crate::input::PoolInput;
use crate::record::Record;
use crate::settings::{Blocked, ProviderSettings};
use crate::time::Millis;

/// The offers of `provider` in `pool` that its `provider_limits` allow: an offer of a type
/// it may not launch is left out, and so is one priced above its most. An offer with no
/// price stays, so that the pick still tells a pool whose only serving offer is unpriced.
fn allowed(pool: &PoolInput, provider: &ProviderSettings) -> Vec<Offer> {
    let offers = pool.offers_of(Some(&provider.provider));
    let types = provider.allowed_instance_types.as_deref();
    let inside = |offer: &&Offer| {
        let typed = types.is_none_or(|types| types.contains(&offer.id));
        let price = offer.price_cents_per_hour.filter(|cents| *cents > 0);
        let priced = price
            .zip(provider.max_price_cents_per_hour)
            .is_none_or(|(cents, max)| cents <= max);
        typed & priced
    };
    offers.iter().filter(inside).cloned().collect()
}

/// The pool's providers as pass 2 sees them, holding `machines` (the records and launches
/// that count against the caps), and the input for a launch for `demand`, given to `then`
/// with the providers' settings in the same order.
fn with_input<R>(
    pool: &PoolInput,
    demand: &Demand,
    machines: &[&Record],
    then: impl FnOnce(&AcquireInput<'_>, &[ProviderSettings]) -> R,
) -> R {
    let count = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
    let providers = pool.settings.providers();
    let offers: Vec<Vec<Offer>> = providers
        .iter()
        .map(|provider| allowed(pool, provider))
        .collect();
    let views: Vec<ProviderView<'_>> = providers
        .iter()
        .zip(&offers)
        .enumerate()
        .map(|(at, (provider, offers))| {
            let own = |record: &&&Record| match record.provider_id() {
                Some(id) => id == provider.provider,
                None => at == 0,
            };
            ProviderView {
                id: provider.provider.clone(),
                enabled: true,
                cooling_down: false,
                braked: false,
                max_machines: provider.share,
                machines: count(machines.iter().filter(own).count()),
                purchase: &[],
                offers,
            }
        })
        .collect();
    let input = AcquireInput {
        demand,
        allowed_instance_types: None,
        max_price_cents_per_hour: None,
        max_machines: None,
        machines: count(machines.len()),
        providers: &views,
    };
    then(&input, &providers)
}

/// The settings of `id`, which pass 2 picked among `providers`; the first's for an id
/// that is none of them, which pass 2 never answers.
fn chosen(providers: &[ProviderSettings], id: &ProviderId) -> ProviderSettings {
    let at = providers.iter().position(|each| &each.provider == id);
    providers[at.unwrap_or_default()].clone()
}

/// The provider and offer pass 2 picks for a launch for `demand` in `pool`, which holds
/// `machines`, with nothing refused: the `launchable` answer. `Err` when it picks none,
/// with what the pool shows for it: `Unpriced` when an offer would serve but has no price,
/// `MaxInstances` when a provider holds its share, and nothing when no offer serves at all.
pub(crate) fn decide(
    pool: &PoolInput,
    demand: &Demand,
    machines: &[&Record],
) -> Result<(ProviderSettings, Offer), Option<Blocked>> {
    with_input(pool, demand, machines, |input, providers| {
        match pick(input) {
            (AcquireDecision::Acquire { provider, offer }, _) => {
                Ok((chosen(providers, &provider), *offer))
            }
            (AcquireDecision::Wait(waiting), skips) => Err(shown(waiting, &skips)),
        }
    })
}

/// The provider and offer pass 2 picks for `request`, for `demand`, at `now`, with the
/// refusals `memory` holds ([`AcquireMemory::next`]): the offers declined for capacity in
/// the request's attempt and the instance types passed over for quota are left out, the
/// other zones of an offer refused for capacity come first, then the provider's other
/// offers, then the next provider's; and a request whose every offer of every provider was
/// refused waits the pool's `acquire_retry`, or until a catalog of the pool was fetched
/// since the last step ([`PoolInput::catalog_refreshed`]). `Err` as [`decide`]; a request
/// that waits for its next attempt shows nothing.
pub(crate) fn walk(
    pool: &PoolInput,
    (request, demand): (&RequestId, &Demand),
    machines: &[&Record],
    memory: &mut AcquireMemory,
    now: Millis,
) -> Result<(ProviderSettings, Offer), Option<Blocked>> {
    let now = Instant::from_millis(now.as_millis());
    let retry = (pool.settings.acquire_retry, pool.catalog_refreshed);
    with_input(pool, demand, machines, |input, providers| {
        match memory.next(request, input, now, retry) {
            (Next::Acquire { provider, offer }, _) => Ok((chosen(providers, &provider), *offer)),
            (Next::Wait(waiting), skips) => Err(shown(waiting, &skips)),
            (Next::Exhausted { .. } | Next::Retrying { .. }, _) => Err(None),
        }
    })
}

/// What the pool shows when pass 2 acquires nothing.
/// The input sets no pool total, so pass 2 never answers [`Waiting::PoolFull`]: a
/// provider's share is the one count it holds the pool to.
fn shown(waiting: Waiting, skips: &[Skip]) -> Option<Blocked> {
    match waiting {
        Waiting::Unpriced => Some(Blocked::Unpriced),
        _ => skips
            .iter()
            .any(|skip| skip.why == SkipReason::OverShare)
            .then_some(Blocked::MaxInstances),
    }
}
