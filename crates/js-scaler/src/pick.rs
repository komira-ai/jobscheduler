//! Pass 2 of placement for one pool (design 5.6, step 5). The scaler does not choose the
//! offer a launch takes: it gives [`js_sched::acquire::pick`] the pool as its one machine
//! provider and stages the offer the answer names, or shows why there is none.
//!
//! A pool has one machine provider today, so the input holds one [`ProviderView`]: the
//! pool's offers (less those its refusals rule out, [`walk`]), the provider's
//! share and the machines the pool holds, and the pool's `provider_limits`. The view is
//! enabled and neither cooling down nor braked. The scaler keeps the pool's cooldown and its
//! brake and applies them itself: a cooling pool launches nothing before the answer is read,
//! and a tripped pool still shows what it would launch. Passing them in too would hide those
//! lines, and turn the `launchable` answer for a cooling pool from yes to no. The
//! pool's total is `max_instances`, which the caps hold, so the input sets none. No
//! configuration key gives a provider a `purchase` order yet, so the view has none and that
//! tie of the pick does not separate two offers.

use js_elastic::{Demand, Instant, Offer};
use js_sched::acquire::{
    AcquireDecision, AcquireInput, ProviderView, Skip, SkipReason, Waiting, pick,
};
use js_sched::failover::{AcquireMemory, Next};
use js_types::RequestId;

use crate::settings::{Blocked, Settings};
use crate::time::Millis;

/// The pool's one provider as pass 2 sees it, holding `machines`, with `offers`, and the
/// input for a launch for `demand`, given to `then`.
fn with_input<R>(
    settings: &Settings,
    offers: &[Offer],
    demand: &Demand,
    machines: usize,
    then: impl FnOnce(&AcquireInput<'_>) -> R,
) -> R {
    let machines = u32::try_from(machines).unwrap_or(u32::MAX);
    let provider = ProviderView {
        id: settings.provider.clone(),
        enabled: true,
        cooling_down: false,
        braked: false,
        max_machines: settings.provider_share,
        machines,
        purchase: &[],
        offers,
    };
    let input = AcquireInput {
        demand,
        allowed_instance_types: settings.allowed_instance_types.as_deref(),
        max_price_cents_per_hour: settings.max_price_cents_per_hour,
        max_machines: None,
        machines,
        providers: std::slice::from_ref(&provider),
    };
    then(&input)
}

/// The offer pass 2 picks for a launch for `demand` in the pool of `settings`, whose
/// provider lists `offers` and holds `machines` (records and launches that count against
/// the caps), with nothing refused: the `launchable` answer. `Err` when it picks
/// none, with what the pool shows for it: `Unpriced` when an offer would serve but has no
/// price, `MaxInstances` when the provider holds its share, and nothing when no offer
/// serves at all.
pub(crate) fn decide(
    settings: &Settings,
    offers: &[Offer],
    demand: &Demand,
    machines: usize,
) -> Result<Offer, Option<Blocked>> {
    with_input(settings, offers, demand, machines, |input| {
        match pick(input) {
            (AcquireDecision::Acquire { offer, .. }, _) => Ok(*offer),
            (AcquireDecision::Wait(waiting), skips) => Err(shown(waiting, &skips)),
        }
    })
}

/// The offer pass 2 picks for `request`, for `demand`, at `now`, with the refusals
/// `memory` holds ([`AcquireMemory::next`]): the offers declined for capacity in the
/// request's attempt and the instance types passed over for quota are left out, the other
/// zones of an offer refused for capacity come first, and a request whose every offer was
/// refused waits the pool's `acquire_retry`, or until `refreshed` (the pool's catalog was
/// fetched since the last step). `Err` as [`decide`]; a request that waits for its next
/// attempt shows nothing.
pub(crate) fn walk(
    settings: &Settings,
    (offers, refreshed): (&[Offer], bool),
    (request, demand): (&RequestId, &Demand),
    machines: usize,
    memory: &mut AcquireMemory,
    now: Millis,
) -> Result<Offer, Option<Blocked>> {
    let now = Instant::from_millis(now.as_millis());
    let retry = (settings.acquire_retry, refreshed);
    with_input(settings, offers, demand, machines, |input| {
        match memory.next(request, input, now, retry) {
            (Next::Acquire { offer, .. }, _) => Ok(*offer),
            (Next::Wait(waiting), skips) => Err(shown(waiting, &skips)),
            (Next::Exhausted { .. } | Next::Retrying { .. }, _) => Err(None),
        }
    })
}

/// What the pool shows when pass 2 acquires nothing.
/// The input sets no pool total, so pass 2 never answers [`Waiting::PoolFull`]: the
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
