//! The instance types as offers, and the choice among offers: the ones that fit a demand,
//! best first.

use std::collections::BTreeSet;

use js_elastic::{Demand, Offer, PriceUnit, ProviderCaps};
use js_types::{Arch, ExecMode, accelerator_of, normalise_arch};

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
/// [`offers`] spells it, and the canonical GPU family of every row of `table`, which is where an
/// offer's accelerator model comes from. An instance type of an architecture Lambda does not document
/// reaches the catalog as its own text; the provider adds those its catalog has carried. A machine runs the agent, so it can be had whole and its
/// jobs may declare host ports.
pub(crate) fn capabilities(table: &OptionTable) -> ProviderCaps {
    ProviderCaps {
        os: BTreeSet::from([OS.to_owned()]),
        arch: DOCUMENTED.iter().map(arch_name).collect(),
        accelerators: table
            .rows()
            .iter()
            .map(|row| row.gpu_class.canonical().model)
            .collect(),
        whole_host: true,
        host_ports: true,
    }
}

/// What is wrong with the rows of `table` against the catalog `types`, one line per row: a
/// row that names no type of the catalog (a typo) or disagrees with it on the GPU count.
pub(crate) fn row_faults(table: &OptionTable, types: &[InstanceTypeOffer]) -> Vec<String> {
    table
        .rows()
        .iter()
        .filter_map(
            |row| match types.iter().find(|kind| kind.name == row.instance_type) {
                None => Some(format!("{} is not offered", row.instance_type)),
                Some(kind) if kind.gpus != row.gpus => Some(format!(
                    "{} has {} gpus in the catalog, not {}",
                    row.instance_type, kind.gpus, row.gpus
                )),
                Some(_) => None,
            },
        )
        .collect()
}

/// One offer per (type, region with capacity now). A type is offered when it is a row of
/// `table` with the GPU count the API gives for it (a row that disagrees with the API is
/// a configuration error and is skipped); the row's class is the offer's GPU class. A
/// type with no region that has capacity has no offer. When `only` is set, only the
/// regions it lists are offered.
pub(crate) fn offers(
    table: &OptionTable,
    types: &[InstanceTypeOffer],
    only: Option<&[String]>,
) -> Vec<Offer> {
    for fault in row_faults(table, types) {
        tracing::warn!(%fault, "a types row is skipped");
    }
    types
        .iter()
        .filter_map(|kind| {
            table
                .get(&kind.name)
                .filter(|row| row.gpus == kind.gpus)
                .map(|row| (kind, row))
        })
        .flat_map(|(kind, row)| {
            let mut regions: Vec<&str> = kind
                .regions
                .iter()
                .map(|r| r.name.as_str())
                .filter(|name| only.is_none_or(|only| only.iter().any(|o| o == name)))
                .collect();
            regions.sort_unstable();
            regions.dedup();
            regions.into_iter().map(|region| Offer {
                id: kind.name.clone(),
                zone: Some(region.to_owned()),
                cpu_millis: u64::from(kind.vcpus) * 1000,
                memory_bytes: u64::from(kind.memory_gib) * GIB,
                disk_bytes: kind.storage_gib.saturating_mul(GIB),
                accelerator: Some(accelerator_of(&row.gpu_class, kind.gpus, None)),
                os: OS.to_owned(),
                arch: arch_name(&kind.architecture),
                // The catalog does not say which ISA level an instance type's CPU reaches,
                // so the offer states none and fits no demand that asks for a level.
                isa_level: None,
                exec: ExecMode::Container,
                price_cents_per_hour: Some(kind.price_cents_per_hour),
                price_unit: PriceUnit::UsCents,
                available: true,
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
mod tests {
    use js_types::Resources;

    use super::*;
    use crate::model::Region;

    fn kind(name: &str, regions: &[&str]) -> InstanceTypeOffer {
        InstanceTypeOffer {
            name: name.to_owned(),
            description: String::new(),
            gpu_description: String::new(),
            price_cents_per_hour: 50,
            vcpus: 8,
            memory_gib: 64,
            storage_gib: 100,
            gpus: 1,
            architecture: Architecture::X86_64,
            regions: regions
                .iter()
                .map(|region| Region {
                    name: (*region).to_owned(),
                    description: String::new(),
                })
                .collect(),
        }
    }

    fn anything() -> Demand {
        Demand {
            resources: Resources::default(),
            disk_bytes: None,
            os: None,
            arch: None,
            isa_level: None,
            exec: None,
            accelerator: None,
            network: Default::default(),
            host_ports: Vec::new(),
        }
    }

    /// Asserts that an offer the catalog marks unavailable is never chosen even though it
    /// fits, and that the same offer is chosen when it is available: the choice reads
    /// availability and the fit separately.
    #[test]
    fn an_unavailable_offer_is_never_chosen() {
        let table = OptionTable::parse(
            "[[option]]\ninstance_type = \"gpu_1x_a\"\ngpu_class = \"nvidia-a\"\ngpus = 1\n",
        )
        .unwrap();
        let mut all = offers(&table, &[kind("gpu_1x_a", &["r1", "r2"])], None);
        assert_eq!(candidates(&all, &anything()).len(), 2);
        all[0].available = false;
        let chosen = candidates(&all, &anything());
        assert_eq!(chosen.len(), 1);
        assert_eq!(chosen[0].zone.as_deref(), Some("r2"));
    }

    /// Asserts that an offer with no price is never chosen ahead of a priced one, even
    /// when its id and zone sort first and its price would read as the smallest.
    #[test]
    fn an_unpriced_offer_is_never_chosen_ahead_of_a_priced_one() {
        let table = OptionTable::parse(
            "[[option]]\ninstance_type = \"gpu_1x_a\"\ngpu_class = \"nvidia-a\"\ngpus = 1\n\
             [[option]]\ninstance_type = \"gpu_1x_b\"\ngpu_class = \"nvidia-a\"\ngpus = 1\n",
        )
        .unwrap();
        let mut all = offers(
            &table,
            &[kind("gpu_1x_a", &["r1"]), kind("gpu_1x_b", &["r1"])],
            None,
        );
        all[0].price_cents_per_hour = None;
        let chosen = candidates(&all, &anything());
        assert_eq!(chosen.len(), 2);
        assert_eq!(chosen[0].id, "gpu_1x_b");
        assert_eq!(chosen[1].id, "gpu_1x_a");
    }
}
