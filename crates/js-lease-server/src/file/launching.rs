//! The launching machine providers of an elastic pool: one for the provider table a pool
//! names, or one for each launching machine provider its platforms reach, in the order they
//! list them (design 5.6, step 5), each with the pool's `provider_limits` on it; and what the
//! pool's providers share, its floor.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use js_elastic::CapacityProvider;
use js_sched::admission_ever::PoolLimits;
use js_types::ProviderId;

use super::limits::{Failover, ProviderLimits};
use super::{ElasticPlan, Registering, Section};
use crate::error::StartupError;
use crate::kinds::{ElasticBuilt, Floor};

/// One machine provider an elastic pool launches with, and the pool's `provider_limits` on
/// it.
pub(crate) struct LaunchingProvider {
    /// Its name in the file: a `[[machine_provider]]` table's, or the one the loader made for
    /// a pool that names its provider directly.
    pub(crate) name: String,
    /// Its provider kind, such as `lambdalabs`.
    pub(crate) kind: String,
    pub(crate) provider: Arc<dyn CapacityProvider>,
    /// How many machines it may hold for the pool (its or the pool's `max_machines`).
    pub(crate) share: u32,
    /// The pool's `provider_limits` on it, as admission reads them.
    pub(crate) limits: PoolLimits,
    /// Pass 2's failover timing: its `provider_limits` `quota_backoff` and `acquire_retry`.
    pub(crate) failover: Failover,
    /// How long its cached catalog is used: its `provider_limits` `catalog_refresh`.
    pub(crate) catalog_refresh: Duration,
    /// How long one of its offers cools down: its `provider_limits` `offer_cooldown`.
    pub(crate) offer_cooldown: Duration,
    /// How far below its offer a hello may be: its `provider_limits` `size_tolerance_percent`.
    pub(crate) size_tolerance_percent: u8,
}

impl LaunchingProvider {
    /// The provider `provider` of kind `kind`, under `name`, with the defaults of
    /// `provider_limits` and a share of `share`.
    pub(super) fn new(
        name: String,
        kind: String,
        provider: Arc<dyn CapacityProvider>,
        share: u32,
    ) -> Self {
        let limits = ProviderLimits::default();
        Self {
            name,
            kind,
            provider,
            share,
            limits: PoolLimits::default(),
            failover: limits.failover(),
            catalog_refresh: limits.catalog_refresh,
            offer_cooldown: limits.offer_cooldown,
            size_tolerance_percent: limits.size_tolerance_percent,
        }
    }
}

impl Registering {
    /// Gives each launching provider its name, `names` in the same order, and its share, the
    /// limits admission reads, the catalog's refresh and cool-down and the hello's size
    /// tolerance, from the pool's `provider_limits` on it (`limits`, by name: a launching
    /// provider with none takes the defaults, and a limit for any other is refused before
    /// this) and the pool's `max_machines`.
    pub(super) fn admit(
        &mut self,
        names: &[String],
        limits: &BTreeMap<String, ProviderLimits>,
        max_machines: Option<u64>,
    ) {
        let providers = self
            .plan
            .iter_mut()
            .flat_map(|plan| plan.providers.iter_mut());
        for (provider, name) in providers.zip(names) {
            let limit = limits.get(name).cloned().unwrap_or_default();
            let share = limit.max_machines.or(max_machines).unwrap_or_default();
            provider.name.clone_from(name);
            provider.share = u32::try_from(share).unwrap_or(u32::MAX);
            provider.limits = PoolLimits {
                allowed_instance_types: limit.allowed_instance_types.clone(),
                max_price_cents_per_hour: limit.max_price_cents_per_hour,
            };
            provider.failover = limit.failover();
            provider.catalog_refresh = limit.catalog_refresh;
            provider.offer_cooldown = limit.offer_cooldown;
            provider.size_tolerance_percent = limit.size_tolerance_percent;
        }
    }
}

impl ElasticPlan {
    /// The providers the pool launches with, in the order pass 2 asks them.
    pub fn providers(&self) -> impl Iterator<Item = &dyn CapacityProvider> {
        self.providers.iter().map(|each| each.provider.as_ref())
    }

    /// The first provider the pool launches with: the one whose checks are the pool's
    /// health and whose notices the scaler reads.
    pub(crate) fn lead(&self) -> &LaunchingProvider {
        &self.providers[0]
    }
}

/// Refuses `built`, the provider of the table `p`, when an earlier provider of the pool
/// among `builts` has its id: the scheduler tells providers apart by their id, which is
/// their kind's.
pub(super) fn distinct(
    builts: &[(String, ElasticBuilt, Section)],
    built: &ElasticBuilt,
    p: &Section,
) -> Result<(), StartupError> {
    let id = built.provider.id();
    match builts.iter().any(|(_, each, _)| each.provider.id() == id) {
        true => Err(p.fail("kind", &same_provider(&id))),
        false => Ok(()),
    }
}

/// The least of `floors`: the fewest GPUs, of the one class they share; no GPU when two of
/// them have GPUs of different classes, or when there are none.
pub(super) fn least_floor<'f>(floors: impl Iterator<Item = &'f Floor>) -> Floor {
    let none = Floor {
        gpus: 0,
        gpu_class: None,
    };
    let least = floors.fold(None, |least: Option<Floor>, each| match least {
        None => Some(each.clone()),
        Some(least) if least.gpu_class == each.gpu_class => Some(Floor {
            gpus: least.gpus.min(each.gpus),
            gpu_class: least.gpu_class,
        }),
        Some(_) => Some(none.clone()),
    });
    least.unwrap_or(none)
}

/// Why a pool reaches the provider of `id` a second time.
fn same_provider(id: &ProviderId) -> String {
    let id = id.as_str();
    format!(
        "its provider is {id:?}, as is that of an earlier machine provider of the pool; a pool reaches each provider once"
    )
}
