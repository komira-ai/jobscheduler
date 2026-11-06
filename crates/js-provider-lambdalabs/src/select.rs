//! The instance types as offers, and the choice among offers: the ones that fit a demand,
//! best first.

use std::collections::BTreeSet;

use js_elastic::{Demand, Offer, PriceUnit, ProviderCaps};
use js_types::{Accelerator, Arch, ExecMode, normalise_arch};

use crate::catalog::{self, Entry};
use crate::model::{Architecture, InstanceTypeOffer};
use crate::options::OptionTable;

const GIB: u64 = 1 << 30;

/// The operating system of every instance type: Lambda launches Linux only.
const OS: &str = "linux";

/// The offer's architecture in the one spelling: `x86_64`, `arm64`, and any other text the
/// catalog gives through [`normalise_arch`], so a catalog that says `aarch64` offers
/// `arm64` and one this crate does not know is kept as the catalog spells it.
fn arch_name(architecture: &Architecture) -> String {
    match architecture {
        Architecture::X86_64 => Arch::X86_64.name().to_owned(),
        Architecture::Arm64 => Arch::Arm64.name().to_owned(),
        Architecture::Other(other) => normalise_arch(other).to_owned(),
    }
}

/// The architectures Lambda documents for an instance type.
const DOCUMENTED: [Architecture; 2] = [Architecture::X86_64, Architecture::Arm64];

/// What the provider could ever offer, from the same sources as [`offers`]: the one
/// operating system, the architecture of every instance type Lambda documents as
/// [`offers`] spells it, and the canonical accelerator model the catalog table gives each
/// type of the allow list `table`, which is where an offer's accelerator comes from. An
/// instance type of an architecture Lambda does not document reaches the catalog as its
/// own text; the provider adds those its catalog has carried. A machine runs the agent, so
/// it can be had whole and its jobs may declare host ports.
pub(crate) fn capabilities(table: &OptionTable) -> ProviderCaps {
    ProviderCaps {
        os: BTreeSet::from([OS.to_owned()]),
        arch: DOCUMENTED.iter().map(arch_name).collect(),
        accelerators: table
            .rows()
            .iter()
            .filter_map(|row| catalog::entry(&row.instance_type))
            .filter_map(Entry::accelerator)
            .map(|accelerator| accelerator.model)
            .collect(),
        whole_host: true,
        host_ports: true,
    }
}

/// What is wrong with the rows of the allow list `table` against Lambda's catalog `types`
/// and the catalog table of this crate, one line per row: a row that names no type Lambda
/// offers (a typo, or a type Lambda withdrew), one the catalog table does not know (a type
/// newer than this release), or one whose GPU count in Lambda's catalog is not the
/// table's (the table is stale for it).
pub(crate) fn row_faults(table: &OptionTable, types: &[InstanceTypeOffer]) -> Vec<String> {
    table
        .rows()
        .iter()
        .filter_map(|row| {
            let name = &row.instance_type;
            let offered = types.iter().find(|kind| &kind.name == name);
            match (offered, catalog::entry(name)) {
                (None, _) => Some(format!("{name} is not offered")),
                (Some(_), None) => Some(format!("{name} is not in the catalog table")),
                (Some(kind), Some(entry)) if kind.gpus != entry.count => Some(format!(
                    "{name} has {} gpus in the catalog, not {}",
                    kind.gpus, entry.count
                )),
                (Some(_), Some(_)) => None,
            }
        })
        .collect()
}

/// The accelerator of the type `kind` when it can be offered: it is on the allow list
/// `table`, the catalog table knows it, and Lambda's catalog gives it the table's GPU
/// count.
fn offered(table: &OptionTable, kind: &InstanceTypeOffer) -> Option<Accelerator> {
    table
        .get(&kind.name)
        .and_then(|row| catalog::entry(&row.instance_type))
        .filter(|entry| entry.count == kind.gpus)
        .and_then(Entry::accelerator)
}

/// One offer per (type, region with capacity now). A type is offered when it is a row of
/// the allow list `table`, the catalog table of this crate knows it, and the API gives it
/// the table's GPU count; its accelerator is the table's. A row that fails any of these is
/// skipped with a warning, and the rows beside it are still offered. A type the table
/// knows and `table` does not list is not offered. A type with no region that has capacity
/// has no offer. When `only` is set, only the regions it lists are offered.
pub(crate) fn offers(
    table: &OptionTable,
    types: &[InstanceTypeOffer],
    only: Option<&[String]>,
) -> Vec<Offer> {
    for fault in row_faults(table, types) {
        tracing::warn!("an instance type of the allow list is skipped: {fault}");
    }
    types
        .iter()
        .filter_map(|kind| offered(table, kind).map(|accelerator| (kind, accelerator)))
        .flat_map(|(kind, accelerator)| {
            let mut regions: Vec<&str> = kind
                .regions
                .iter()
                .map(|r| r.name.as_str())
                .filter(|name| only.is_none_or(|only| only.iter().any(|o| o == name)))
                .collect();
            regions.sort_unstable();
            regions.dedup();
            regions.into_iter().map(move |region| Offer {
                id: kind.name.clone(),
                zone: Some(region.to_owned()),
                cpu_millis: u64::from(kind.vcpus) * 1000,
                memory_bytes: u64::from(kind.memory_gib) * GIB,
                disk_bytes: kind.storage_gib.saturating_mul(GIB),
                accelerator: Some(accelerator.clone()),
                os: OS.to_owned(),
                arch: arch_name(&kind.architecture),
                // The catalog does not say which ISA level an instance type's CPU reaches,
                // so the offer states none and fits no demand that asks for a level.
                isa_level: None,
                exec: ExecMode::Container,
                price_cents_per_hour: Some(kind.price_cents_per_hour),
                price_unit: PriceUnit::UsCents,
                available: true,
                purchase: Some(js_elastic::Purchase::OnDemand),
                quota: None,
                block: None,
            })
        })
        .collect()
}

/// The offers that are available and fit `demand`, in the order to try them: cheapest
/// first (an offer with no price last), then fewest GPUs, then id, then zone. The price
/// orders and never excludes. No offer means no capacity.
pub(crate) fn candidates(offers: &[Offer], demand: &Demand) -> Vec<Offer> {
    let mut fits: Vec<&Offer> = offers
        .iter()
        .filter(|offer| {
            [offer.available, offer.fit(demand).is_ok()]
                .into_iter()
                .all(|ok| ok)
        })
        .collect();
    // An unpriced offer sorts after every priced one: no price is not a cheap price.
    let key = |o: &Offer| {
        (
            o.price_cents_per_hour.is_none(),
            o.price_cents_per_hour,
            o.accelerator.as_ref().map_or(0, |a| a.count),
        )
    };
    fits.sort_by(|a, b| {
        key(a)
            .cmp(&key(b))
            .then_with(|| (&a.id, &a.zone).cmp(&(&b.id, &b.zone)))
    });
    fits.into_iter().cloned().collect()
}

#[cfg(test)]
#[path = "select_tests.rs"]
mod tests;
