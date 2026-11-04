//! A pool's `[pool.provider_limits.<name>]` tables: what the pool lets each machine
//! provider of its platforms launch (design 4.2).
//!
//! They are read and checked at start, and nothing enforces them yet: the pick of an offer
//! when a pool acquires a machine (design 5.6, pass 2) is what will read them. A static
//! machine provider launches nothing, so a limit for one is refused.

use std::collections::BTreeMap;

use super::Section;
use crate::error::StartupError;

const KEYS: &[&str] = &[
    "allowed_instance_types",
    "max_price_cents_per_hour",
    "max_machines",
    "max_spot_machines",
];

/// What a pool lets one machine provider launch. Read and checked at start; not enforced
/// yet (the pick of an offer in design 5.6, pass 2, will read it).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
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
