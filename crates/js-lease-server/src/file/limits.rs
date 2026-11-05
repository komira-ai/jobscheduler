//! A pool's `[pool.provider_limits.<name>]` tables: what the pool lets each machine
//! provider of its platforms launch (design 4.2).
//!
//! They are read and checked at start. Admission reads `allowed_instance_types`,
//! `max_price_cents_per_hour` and `max_machines` (an offer outside them does not admit a
//! request, design 5.6, step 2); the pick of an offer when a pool acquires a machine
//! (design 5.6, pass 2) does not read them yet. `catalog_refresh` is how long the cached
//! catalog of the provider is used before it is fetched again, and `offer_cooldown` how long
//! one of its offers cools down once one is (nothing enters a cool-down yet); each defaults
//! to one hour and must not be zero. A static
//! machine provider launches nothing, so a limit for one is refused.

use std::collections::BTreeMap;
use std::time::Duration;

use super::Section;
use crate::error::StartupError;

const KEYS: &[&str] = &[
    "allowed_instance_types",
    "max_price_cents_per_hour",
    "max_machines",
    "max_spot_machines",
    "catalog_refresh",
    "offer_cooldown",
];

/// The `catalog_refresh` of a provider whose table does not give it.
const DEFAULT_CATALOG_REFRESH: Duration = Duration::from_secs(3600);

/// The `offer_cooldown` of a provider whose table does not give it.
const DEFAULT_OFFER_COOLDOWN: Duration = Duration::from_secs(3600);

/// What a pool lets one machine provider launch. Read and checked at start, and read by
/// admission and by the cache of the provider's catalog; the pick of an offer (design 5.6,
/// pass 2) does not read it yet.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ProviderLimits {
    /// The instance types the provider may launch for the pool, in the file's order;
    /// `None`: any.
    pub allowed_instance_types: Option<Vec<String>>,
    /// The highest price of a machine it may launch, in cents per hour; `None`: any.
    pub max_price_cents_per_hour: Option<u64>,
    /// The most machines it may hold for the pool at once, a share of the pool's
    /// `max_machines`; `None`: up to the pool's.
    pub max_machines: Option<u64>,
    /// The most of those that may be spot machines; `None`: no bound of its own.
    pub max_spot_machines: Option<u64>,
    /// How long the provider's cached catalog is used before it is fetched again
    /// (`catalog_refresh`); one hour when not given. Never zero.
    pub catalog_refresh: Duration,
    /// How long one of the provider's offers cools down once it is cooled down
    /// (`offer_cooldown`); one hour when not given. Never zero.
    pub offer_cooldown: Duration,
}

impl Default for ProviderLimits {
    /// No limit, and the default refresh and cool-down.
    fn default() -> Self {
        Self {
            allowed_instance_types: None,
            max_price_cents_per_hour: None,
            max_machines: None,
            max_spot_machines: None,
            catalog_refresh: DEFAULT_CATALOG_REFRESH,
            offer_cooldown: DEFAULT_OFFER_COOLDOWN,
        }
    }
}

/// A machine provider a pool's platforms reach, as the limits read it.
pub(super) struct Reached<'a> {
    /// Its name.
    pub(super) name: &'a str,
    /// Whether it launches machines; a static provider does not.
    pub(super) launches: bool,
}

/// The limits of `table`, the pool's `provider_limits`, keyed by machine provider. `head`
/// is the pool's (`pool "gpu": `), `reached` the machine providers of its platforms, and
/// `cap` its `max_machines`, when it has one.
pub(super) fn read(
    table: toml::Table,
    head: &str,
    reached: &[Reached<'_>],
    cap: Option<u64>,
) -> Result<BTreeMap<String, ProviderLimits>, StartupError> {
    let outer = Section::open(toml::Table::new(), format!("{head}provider_limits."));
    let mut limits = BTreeMap::new();
    for (name, value) in table {
        let toml::Value::Table(entry) = value else {
            return Err(outer.fail(&name, "must be a table"));
        };
        match reached.iter().find(|reach| reach.name == name) {
            None => {
                let reason = "not a machine provider of the pool's platforms";
                return Err(outer.fail(&name, reason));
            }
            Some(reach) if !reach.launches => {
                let reason =
                    "a static machine provider launches nothing, so it has nothing to limit";
                return Err(outer.fail(&name, reason));
            }
            Some(_) => {}
        }
        let mut s = Section::open(entry, format!("{head}provider_limits.{name}."));
        s.only(KEYS)?;
        let read = ProviderLimits {
            allowed_instance_types: s.names("allowed_instance_types")?,
            max_price_cents_per_hour: at_least_one(&mut s, "max_price_cents_per_hour")?,
            max_machines: at_least_one(&mut s, "max_machines")?,
            max_spot_machines: s.integer("max_spot_machines")?,
            catalog_refresh: s.duration_or("catalog_refresh", DEFAULT_CATALOG_REFRESH)?,
            offer_cooldown: s.duration_or("offer_cooldown", DEFAULT_OFFER_COOLDOWN)?,
        };
        if let (Some(share), Some(cap)) = (read.max_machines, cap)
            && share > cap
        {
            let reason = format!("must not be above the pool's max_machines ({cap})");
            return Err(s.fail("max_machines", &reason));
        }
        if let (Some(spot), Some(most)) = (read.max_spot_machines, read.max_machines)
            && spot > most
        {
            let reason = format!("must not be above max_machines ({most})");
            return Err(s.fail("max_spot_machines", &reason));
        }
        limits.insert(name, read);
    }
    Ok(limits)
}

/// The integer `key` when the table gives it, which must be 1 or more.
fn at_least_one(s: &mut Section, key: &str) -> Result<Option<u64>, StartupError> {
    match s.integer(key)? {
        Some(0) => Err(s.fail(key, "must be at least 1")),
        given => Ok(given),
    }
}
