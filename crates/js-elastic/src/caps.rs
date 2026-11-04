//! What a provider could ever offer, and how many machines it holds and could add: the
//! answers a scheduler reads to refuse a job no machine of the provider could serve, and
//! to know how far the provider can grow, without a call to the cloud.

use std::collections::BTreeSet;

use crate::offer::{GpuClass, Offer};

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
    /// The GPU classes, as [`Offer::gpu_class`] spells them.
    pub accelerators: BTreeSet<GpuClass>,
    /// Whether a job may have one of its machines whole.
    pub whole_host: bool,
    /// Whether a job on one of its machines may declare host ports.
    pub host_ports: bool,
}

impl ProviderCaps {
    /// Whether `offer` is inside these capabilities: its operating system and architecture
    /// are in the sets, and so is its GPU class when it has one.
    pub fn covers(&self, offer: &Offer) -> bool {
        let class = offer
            .gpu_class
            .as_ref()
            .is_none_or(|class| self.accelerators.contains(class));
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
    use js_types::ExecMode;

    use super::*;
    use crate::offer::PriceUnit;

    fn offer(os: &str, arch: &str, class: Option<&str>) -> Offer {
        Offer {
            id: "type".to_owned(),
            zone: None,
            cpu_millis: 1,
            memory_bytes: 1,
            disk_bytes: 1,
            gpus: u32::from(class.is_some()),
            gpu_class: class.map(|class| GpuClass::parse(class).unwrap()),
            os: os.to_owned(),
            arch: arch.to_owned(),
            isa_level: None,
            exec: ExecMode::Container,
            price_cents_per_hour: None,
            price_unit: PriceUnit::UsCents,
            available: true,
        }
    }

    fn caps() -> ProviderCaps {
        ProviderCaps {
            os: BTreeSet::from(["linux".to_owned()]),
            arch: BTreeSet::from(["x86_64".to_owned()]),
            accelerators: BTreeSet::from([GpuClass::parse("nvidia-a10").unwrap()]),
            whole_host: true,
            host_ports: true,
        }
    }

    /// Asserts that an offer is covered when its platform and GPU class are in the sets,
    /// that one with no GPU needs no class, and that an operating system, an architecture
    /// or a GPU class outside the sets each makes it not covered on its own. It catches a
    /// check that skips one of the three, which would let a catalog drift from the
    /// capabilities unseen.
    #[test]
    fn an_offer_is_covered_only_when_each_of_its_strings_is_in_the_sets() {
        let caps = caps();
        assert!(caps.covers(&offer("linux", "x86_64", Some("nvidia-a10"))));
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
