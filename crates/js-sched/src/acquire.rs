//! Pass 2 of the placement walk (design 5.6, step 5): which machine provider is asked to
//! launch a machine for a job, and for which of its offers.
//!
//! [`pick`] answers that question and nothing else. Before asking it, the caller has
//! established that nothing can serve the job now: no free capable host in any machine
//! provider of the pool (pass 1 found none), and no machine already booting that can serve
//! it. [`needs_acquire`] is that gate, kept apart from [`pick`]: [`pick`] does not check
//! either, and a caller that asks it while one exists launches a machine for a job that did
//! not need one.
//!
//! Everything comes in as values ([`AcquireInput`], one [`ProviderView`] per machine
//! provider, in the pool's order): the job's demand, the pool's limits, the machines the pool
//! and each provider hold, counted in every state (in-flight acquires and tainted hosts
//! included, design 4.2), each provider's boot-failure cool-down and failure brake as
//! flags, and its catalog as the caller read it, with the offers in an offer cool-down
//! already left out. It reads no clock, calls no provider and records nothing; failing over
//! on a refusal, retrying and the queue wait are the caller's.

use js_elastic::{Demand, Offer, Purchase};
use js_types::ProviderId;

/// Whether pass 2 runs for a job (design 5.6, step 5, first sentence): only when no free
/// capable host exists in any machine provider of the pool and no machine on its way can
/// serve the job.
///
/// `free_capable_hosts` counts the free hosts of every provider that fit the job, not those
/// of one provider. `booting_machines_that_fit` counts the machines on their way whose offer,
/// the one each was acquired for, fits the job ([`Offer::fit`]); a booting machine whose offer
/// does not fit is not counted, and does not hold the job back. One of either is enough to
/// serve the job without a new machine.
#[must_use]
pub fn needs_acquire(free_capable_hosts: usize, booting_machines_that_fit: usize) -> bool {
    free_capable_hosts.saturating_add(booting_machines_that_fit) == 0
}

/// What pass 2 is asked about one job.
#[derive(Debug, Clone, Copy)]
pub struct AcquireInput<'a> {
    /// What the job needs of a host, as [`Offer::fit`] takes it.
    pub demand: &'a Demand,
    /// The instance types (offer ids) the pool may launch. `None`: any.
    pub allowed_instance_types: Option<&'a [String]>,
    /// The highest price per hour, in cents, the pool may launch at; an offer at exactly
    /// this price is inside. `None`: any price.
    pub max_price_cents_per_hour: Option<u64>,
    /// The most machines the pool may hold, over every provider. `None`: no pool total.
    pub max_machines: Option<u32>,
    /// The machines the pool holds, in every state and of every provider, in-flight
    /// acquires and tainted hosts included.
    pub machines: u32,
    /// The pool's machine providers, first to last.
    pub providers: &'a [ProviderView<'a>],
}

/// One machine provider of the pool, as pass 2 sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderView<'a> {
    /// The provider.
    pub id: ProviderId,
    /// Whether it may launch for the pool. No configuration key sets this yet; a caller
    /// gives `true` for every declared machine provider.
    pub enabled: bool,
    /// Whether its boot-failure cool-down for this pool is running.
    pub cooling_down: bool,
    /// Whether the pool's failure brake is tripped for it.
    pub braked: bool,
    /// Its share of the pool: the most machines it may hold for the pool. `None`: no
    /// share of its own, only the pool's total.
    pub max_machines: Option<u32>,
    /// The machines it holds for the pool, counted as [`AcquireInput::machines`] counts.
    pub machines: u32,
    /// Its configured `purchase` order, first preferred.
    pub purchase: &'a [Purchase],
    /// Its catalog, without the offers in an offer cool-down.
    pub offers: &'a [Offer],
}

/// What pass 2 decided for the job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcquireDecision {
    /// Ask `provider` for one machine of `offer`.
    Acquire {
        /// The provider to ask.
        provider: ProviderId,
        /// The offer to acquire, as its catalog gave it. Boxed so that a decision to wait
        /// is not the size of an offer.
        offer: Box<Offer>,
    },
    /// Acquire nothing now; the job stays queued.
    Wait(Waiting),
}

/// Why pass 2 acquires nothing now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Waiting {
    /// Every provider was passed over or had no usable offer.
    NoOffer,
    /// The pool holds its `max_machines`.
    PoolFull,
    /// Some offer would serve the job but has no price, or a price of zero, and so is
    /// never taken.
    Unpriced,
}

/// A provider that pass 2 passed over, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skip {
    /// The provider.
    pub provider: ProviderId,
    /// Why it was passed over.
    pub why: SkipReason,
}

/// Why pass 2 passed a provider over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// It is not enabled.
    Disabled,
    /// Its boot-failure cool-down is running.
    CoolingDown,
    /// The pool's failure brake is tripped for it.
    Braked,
    /// It holds as many machines as its share allows.
    OverShare,
    /// None of its offers is available, fits the job, is inside the pool's limits and has
    /// a price.
    NoUsableOffer,
}

/// How an offer ranks against the others of its provider: the smallest is picked. The
/// fields are compared in their order.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Rank<'o> {
    /// The price per hour.
    price: u64,
    /// The number of accelerators.
    accelerators: u32,
    /// The CPU beyond the demand's.
    spare_cpu_millis: u64,
    /// The memory beyond the demand's.
    spare_memory_bytes: u64,
    /// The position of its purchase option in the provider's `purchase` order; the
    /// length of the order when it has none or one not listed.
    purchase: usize,
    /// The offer id.
    id: &'o str,
    /// The zone.
    zone: Option<&'o str>,
}

/// The provider and offer pass 2 acquires from for the job, or why it acquires nothing,
/// with the providers it passed over on the way, in provider order.
///
/// The policy is the design's (5.6, step 5): walk the machine providers in order. For
/// each, take the offers from its catalog that satisfy the job, are inside the pool's
/// limits and are available. The first provider that has any, is enabled, not cooling
/// down, not braked, and fits under its share of `max_machines` and the pool's, gets one
/// acquire for the demand, for the offer picked by policy: provider order first (the walk
/// itself, so a cheaper offer of a later provider never beats an earlier provider that has
/// a usable offer), then price per hour, then the fewest accelerators, then the least spare
/// CPU and memory, then the purchase order of the provider (`purchase`), then the offer id.
/// An offer with no price is dropped. A provider with no usable offer is passed over and a
/// later one is asked only then.
///
/// In detail, for each provider in order:
///
/// 1. It is passed over when it is not enabled, then when it is cooling down, then when it
///    is braked ([`SkipReason::Disabled`], [`SkipReason::CoolingDown`],
///    [`SkipReason::Braked`]).
/// 2. When the pool holds its `max_machines`, the walk stops with [`Waiting::PoolFull`].
///    Otherwise the provider is passed over when it holds its own share
///    ([`SkipReason::OverShare`]).
/// 3. Its usable offers are those available, fitting the demand ([`Offer::fit`], which
///    compares canonical accelerator models, never their spelling), whose id is in
///    `allowed_instance_types` when that is set, priced above zero, and priced at most
///    `max_price_cents_per_hour` when that is set.
/// 4. The smallest usable offer is acquired, ordered by price, then accelerator count, then
///    spare CPU (the offer's less the demand's), then spare memory, then the position of the
///    offer's purchase option in the provider's `purchase` order (an option not listed, or
///    none, after every listed one), then the offer id, then the zone, so the same inputs
///    give the same answer in any order of the catalog.
/// 5. With no usable offer it is passed over ([`SkipReason::NoUsableOffer`]).
///
/// When no provider gets the acquire, the answer is [`Waiting::Unpriced`] if an offer of a
/// provider that reached step 3 was available, fitting and of an allowed type but had no
/// price or a price of zero, and [`Waiting::NoOffer`] otherwise.
#[must_use]
pub fn pick(input: &AcquireInput<'_>) -> (AcquireDecision, Vec<Skip>) {
    let mut skips = Vec::new();
    let mut unpriced = false;
    for provider in input.providers {
        if let Some(why) = barred(provider) {
            skips.push(skip(provider, why));
            continue;
        }
        if input.max_machines.is_some_and(|max| input.machines >= max) {
            return (AcquireDecision::Wait(Waiting::PoolFull), skips);
        }
        if provider
            .max_machines
            .is_some_and(|share| provider.machines >= share)
        {
            skips.push(skip(provider, SkipReason::OverShare));
            continue;
        }
        let serving: Vec<&Offer> = provider
            .offers
            .iter()
            .filter(|offer| serves(input, offer))
            .collect();
        unpriced |= serving.iter().any(|offer| price(offer).is_none());
        let chosen = serving
            .into_iter()
            .filter_map(|offer| price(offer).map(|cents| (cents, offer)))
            .filter(|&(cents, _)| {
                input
                    .max_price_cents_per_hour
                    .is_none_or(|max| cents <= max)
            })
            .min_by_key(|&(cents, offer)| rank(input.demand, provider.purchase, cents, offer));
        match chosen {
            Some((_, offer)) => {
                let decision = AcquireDecision::Acquire {
                    provider: provider.id.clone(),
                    offer: Box::new(offer.clone()),
                };
                return (decision, skips);
            }
            None => skips.push(skip(provider, SkipReason::NoUsableOffer)),
        }
    }
    let waiting = match unpriced {
        true => Waiting::Unpriced,
        false => Waiting::NoOffer,
    };
    (AcquireDecision::Wait(waiting), skips)
}

/// Why `provider` may not launch at all now, in the order of the walk, or `None`.
fn barred(provider: &ProviderView<'_>) -> Option<SkipReason> {
    [
        (!provider.enabled, SkipReason::Disabled),
        (provider.cooling_down, SkipReason::CoolingDown),
        (provider.braked, SkipReason::Braked),
    ]
    .into_iter()
    .find(|&(holds, _)| holds)
    .map(|(_, why)| why)
}

/// The skip of `provider` for `why`.
fn skip(provider: &ProviderView<'_>, why: SkipReason) -> Skip {
    Skip {
        provider: provider.id.clone(),
        why,
    }
}

/// Whether `offer` would serve the job but for its price: available, fitting the demand,
/// and of an allowed instance type.
fn serves(input: &AcquireInput<'_>, offer: &Offer) -> bool {
    let allowed = input
        .allowed_instance_types
        .is_none_or(|types| types.contains(&offer.id));
    offer.available & offer.fit(input.demand).is_ok() & allowed
}

/// The price of `offer` per hour, or `None` when it has none or a price of zero: a price of
/// zero is no price.
fn price(offer: &Offer) -> Option<u64> {
    offer.price_cents_per_hour.filter(|price| *price > 0)
}

/// How `offer`, at `price`, ranks for `demand` under the provider's `purchase` order.
fn rank<'o>(demand: &Demand, purchase: &[Purchase], price: u64, offer: &'o Offer) -> Rank<'o> {
    let wanted = &demand.resources;
    Rank {
        price,
        accelerators: offer.accelerator.as_ref().map_or(0, |a| a.count),
        spare_cpu_millis: offer.cpu_millis.saturating_sub(wanted.cpu_millis()),
        spare_memory_bytes: offer.memory_bytes.saturating_sub(wanted.memory_bytes()),
        purchase: offer
            .purchase
            .as_ref()
            .and_then(|option| purchase.iter().position(|each| each == option))
            .unwrap_or(purchase.len()),
        id: &offer.id,
        zone: offer.zone.as_deref(),
    }
}
