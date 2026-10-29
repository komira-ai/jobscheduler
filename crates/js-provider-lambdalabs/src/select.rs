//! The instance types as offers, and the choice among offers: the ones that fit a demand,
//! best first.

use js_elastic::{Demand, Offer, PriceUnit};
use js_types::ExecMode;

use crate::model::{Architecture, InstanceTypeOffer};
use crate::options::OptionTable;

const GIB: u64 = 1 << 30;

/// The operating system of every instance type: Lambda launches Linux only.
const OS: &str = "linux";

fn arch_name(architecture: &Architecture) -> String {
    match architecture {
        Architecture::X86_64 => "x86_64".to_owned(),
        Architecture::Arm64 => "aarch64".to_owned(),
        Architecture::Other(other) => other.clone(),
    }
}

/// One offer per (type, region with capacity now). A type is offered when it is a row of
/// `table` with the GPU count the API gives for it (a row that disagrees with the API is
/// a configuration error and is skipped); the row's class is the offer's GPU class. A
/// type with no region that has capacity has no offer.
pub(crate) fn offers(table: &OptionTable, types: &[InstanceTypeOffer]) -> Vec<Offer> {
    types
        .iter()
        .filter_map(|kind| {
            table
                .get(&kind.name)
                .filter(|row| row.gpus == kind.gpus)
                .map(|row| (kind, row))
        })
        .flat_map(|(kind, row)| {
            let mut regions: Vec<&str> = kind.regions.iter().map(|r| r.name.as_str()).collect();
            regions.sort_unstable();
            regions.dedup();
            regions.into_iter().map(|region| Offer {
                id: kind.name.clone(),
                zone: Some(region.to_owned()),
                cpu_millis: u64::from(kind.vcpus) * 1000,
                memory_bytes: u64::from(kind.memory_gib) * GIB,
                disk_bytes: kind.storage_gib.saturating_mul(GIB),
                gpus: kind.gpus,
                gpu_class: Some(row.gpu_class.clone()),
                os: OS.to_owned(),
                arch: arch_name(&kind.architecture),
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
            o.gpus,
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
            exec: None,
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
        let mut all = offers(&table, &[kind("gpu_1x_a", &["r1", "r2"])]);
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
        );
        all[0].price_cents_per_hour = None;
        let chosen = candidates(&all, &anything());
        assert_eq!(chosen.len(), 2);
        assert_eq!(chosen[0].id, "gpu_1x_b");
        assert_eq!(chosen[1].id, "gpu_1x_a");
    }
}
