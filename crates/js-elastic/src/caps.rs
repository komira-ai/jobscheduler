//! What a provider could ever offer, and how many machines it holds and could add: the
//! answers a scheduler reads to refuse a job no machine of the provider could serve, and
//! to know how far the provider can grow, without a call to the cloud.

use std::collections::BTreeSet;

use js_types::Model;

use crate::offer::Offer;

/// What a provider could ever offer, in the strings an [`Offer`] carries: a job that needs
/// something outside these sets can never be served by it, however long it waits. The
/// sets bound the catalog: every offer the catalog returns is inside them
/// ([`ProviderCaps::covers`]), and an offer that is unavailable now is inside them too.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProviderCaps {
    /// The operating systems, as [`Offer::os`] spells them (`linux`, `macos`).
    pub os: BTreeSet<String>,
    /// The CPU architectures, as [`Offer::arch`] spells them (`x86_64`, `arm64`).
    pub arch: BTreeSet<String>,
    /// The accelerator models, canonical (the family the alias table resolves a spelling
    /// to, as an [`Offer::accelerator`] holds it; a name the table does not know as
    /// itself).
    pub accelerators: BTreeSet<Model>,
    /// Whether a job may have one of its machines whole.
    pub whole_host: bool,
    /// Whether a job on one of its machines may declare host ports.
    pub host_ports: bool,
}

impl ProviderCaps {
    /// Whether `offer` is inside these capabilities: its operating system and architecture
    /// are in the sets, and so is its accelerator model when it has one.
    pub fn covers(&self, offer: &Offer) -> bool {
        let class = offer
            .accelerator
            .as_ref()
            .is_none_or(|accelerator| self.accelerators.contains(&accelerator.model));
        [
            self.os.contains(&offer.os),
            self.arch.contains(&offer.arch),
            class,
        ]
        .into_iter()
        .all(|inside| inside)
    }
}

/// How many machines a provider holds for this scheduler and how many more it could add,
/// from its own records: what it last listed, launched or released. Reading it makes no
/// call to the cloud.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capacity {
    /// The machines it holds that are not ending: launched, booting or up, and not
    /// terminating, terminated or reclaimed.
    pub supplied: u32,
    /// How many more machines it could launch before its own bound (such as a
    /// `max_machines` setting) refuses one; `None` when it knows no bound. A launch can
    /// still be refused for capacity or quota before the bound is reached.
    pub can_add: Option<u32>,
}

#[cfg(test)]
mod tests {
    use js_types::{ExecMode, accelerator_of};

    use super::*;
    use crate::offer::PriceUnit;

    fn offer(os: &str, arch: &str, class: Option<&str>) -> Offer {
        Offer {
            id: "type".to_owned(),
            zone: None,
            cpu_millis: 1,
            memory_bytes: 1,
            disk_bytes: 1,
            accelerator: class.map(|class| accelerator_of(&Model::parse(class).unwrap(), 1, None)),
            os: os.to_owned(),
            arch: arch.to_owned(),
            isa_level: None,
            exec: ExecMode::Container,
            price_cents_per_hour: None,
            price_unit: PriceUnit::UsCents,
            available: true,
            purchase: None,
            quota: None,
            block: None,
        }
    }

    fn caps() -> ProviderCaps {
        ProviderCaps {
            os: BTreeSet::from(["linux".to_owned()]),
            arch: BTreeSet::from(["x86_64".to_owned()]),
            accelerators: BTreeSet::from([Model::parse("nvidia-a100").unwrap()]),
            whole_host: true,
            host_ports: true,
        }
    }

    /// Asserts that an offer is covered when its platform and accelerator model are in the
    /// sets (a variant by its family), that one with no GPU needs no model, and that an
    /// operating system, an architecture or a model outside the sets each makes it not
    /// covered on its own. It catches a
    /// check that skips one of the three, which would let a catalog drift from the
    /// capabilities unseen.
    #[test]
    fn an_offer_is_covered_only_when_each_of_its_strings_is_in_the_sets() {
        let caps = caps();
        assert!(caps.covers(&offer("linux", "x86_64", Some("nvidia-a100"))));
        assert!(
            caps.covers(&offer("linux", "x86_64", Some("nvidia-a100-sxm4-80gb"))),
            "a variant is covered by its family"
        );
        assert!(caps.covers(&offer("linux", "x86_64", None)));
        assert!(!caps.covers(&offer("macos", "x86_64", None)));
        assert!(!caps.covers(&offer("linux", "arm64", None)));
        assert!(!caps.covers(&offer("linux", "x86_64", Some("nvidia-h100"))));
        assert!(!ProviderCaps::default().covers(&offer("linux", "x86_64", None)));
    }

    /// Asserts that capacity answers compare by value, an unknown bound included.
    #[test]
    fn a_capacity_compares_by_value() {
        let known = Capacity {
            supplied: 2,
            can_add: Some(0),
        };
        assert_eq!(
            known,
            Capacity {
                supplied: 2,
                can_add: Some(0)
            }
        );
        assert_ne!(
            known,
            Capacity {
                supplied: 2,
                can_add: None
            }
        );
    }
}
