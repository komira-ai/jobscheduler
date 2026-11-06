//! Pass 2 of placement for one pool (design 5.6, step 5). The scaler does not choose the
//! offer a launch takes: it gives [`js_sched::acquire::pick`] the pool as its one machine
//! provider and stages the offer the answer names, or shows why there is none.
//!
//! A pool has one machine provider today, so the input holds one [`ProviderView`]: the
//! pool's offers less those a provider refused for capacity in this step, the provider's
//! share and the machines the pool holds, and the pool's `provider_limits`. The view is
//! enabled and neither cooling down nor braked. The scaler keeps the pool's cooldown and its
//! brake and applies them itself: a cooling pool launches nothing before the answer is read,
//! and a tripped pool still shows what it would launch. Passing them in too would hide those
//! lines, and turn the registry's `launchable` answer for a cooling pool from yes to no. The
//! pool's total is `max_instances`, which the caps hold, so the input sets none. No
//! configuration key gives a provider a `purchase` order yet, so the view has none and that
//! tie of the pick does not separate two offers.

use js_elastic::{Demand, Offer};
use js_sched::acquire::{
    AcquireDecision, AcquireInput, ProviderView, Skip, SkipReason, Waiting, pick,
};

use crate::settings::{Blocked, Settings};

/// An offer a provider refused for capacity in this step: the pool, the offer id and the
/// zone. It is not picked again in the same step.
pub(crate) type Declined = (String, String, Option<String>);

/// The offer pass 2 picks for a launch for `demand` in the pool of `settings`, whose
/// provider lists `offers` and holds `machines` (records and launches that count against
/// the caps). `Err` when it picks none, with what the pool shows for it: `Unpriced` when an
/// offer would serve but has no price, `MaxInstances` when the provider holds its share,
/// and nothing when no offer serves at all.
pub(crate) fn decide(
    settings: &Settings,
    offers: &[Offer],
    declined: &[Declined],
    demand: &Demand,
    machines: usize,
) -> Result<Offer, Option<Blocked>> {
    let kept: Vec<Offer> = offers
        .iter()
        .filter(|offer| {
            !declined.iter().any(|(pool, id, zone)| {
                (pool.as_str(), id.as_str(), zone.as_deref())
                    == (
                        settings.pool.as_str(),
                        offer.id.as_str(),
                        offer.zone.as_deref(),
                    )
            })
        })
        .cloned()
        .collect();
    let machines = u32::try_from(machines).unwrap_or(u32::MAX);
    let provider = ProviderView {
        id: settings.provider.clone(),
        enabled: true,
        cooling_down: false,
        braked: false,
        max_machines: settings.provider_share,
        machines,
        purchase: &[],
        offers: &kept,
    };
    let input = AcquireInput {
        demand,
        allowed_instance_types: settings.allowed_instance_types.as_deref(),
        max_price_cents_per_hour: settings.max_price_cents_per_hour,
        max_machines: None,
        machines,
        providers: std::slice::from_ref(&provider),
    };
    match pick(&input) {
        (AcquireDecision::Acquire { offer, .. }, _) => Ok(*offer),
        (AcquireDecision::Wait(waiting), skips) => Err(shown(waiting, &skips)),
    }
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
