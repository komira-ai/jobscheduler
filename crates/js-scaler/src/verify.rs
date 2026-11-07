//! Verify on hello (design 5.6.1, step 6): the rule that compares the shape a launched
//! machine's agent announced in its hello with the offer the machine was acquired for.
//!
//! [`verify_hello`] is pure and judges six dimensions, each on its own, and names every one
//! that failed:
//!
//! * accelerator model: the offer's class is resolved through the alias table
//!   ([`Model::canonical`], via [`AcceleratorNeed::met_by`]) and compared with the model the
//!   hello reports, never as raw text. A family offer (`nvidia-a100`) is met by any variant
//!   of the family; a variant offer (`nvidia-a100-sxm4-80gb`) only by that variant, so a
//!   40 GB host of an 80 GB offer does not match;
//! * accelerator count: at least the offer's; more matches. No tolerance applies;
//! * architecture: equal after the one spelling ([`normalise_arch`]: `aarch64` is `arm64`);
//! * CPU, memory and disk: each at least the offer's less `tolerance_percent`, because an
//!   agent measures a little less than the nominal size. The bound is
//!   `hello * 100 >= offer * (100 - tolerance)`, in 128-bit integers, so it neither rounds
//!   nor overflows for any `u64`.
//!
//! An offer with no accelerator accepts a hello with any count and any model. A dimension
//! the hello does not state is not judged: an architecture it leaves empty, and a disk of
//! zero (the agent reports no disk unless it is given a `disk-gib` capability, and a host
//! that states none offers none, see the server's `offer_of`). Neither is a stand-in for
//! "less than the offer": judging them would release every healthy machine whose agent does
//! not report them. An offer class that is not a model name (a records file edited by hand)
//! states no model to compare. The operating system and the execution mode are not
//! compared: the pool's floor holds the hello to them before the registry admits it.

use std::fmt;

use js_elastic::Offer;
use js_types::{AcceleratorNeed, Model, normalise_arch};
use serde::{Deserialize, Serialize};

use crate::record::StoredOffer;

/// One way a hello can fail its offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HelloDimension {
    /// The accelerator model, compared by canonical name after alias resolution.
    AcceleratorModel,
    /// The number of accelerators: fewer than the offer's.
    AcceleratorCount,
    /// The CPU architecture.
    Architecture,
    /// CPU, below the offer's less the tolerance.
    Cpu,
    /// Memory, below the offer's less the tolerance.
    Memory,
    /// Disk, below the offer's less the tolerance.
    Disk,
}

impl HelloDimension {
    /// The name the records file, the status view and the logs use: `accelerator_model`,
    /// `accelerator_count`, `architecture`, `cpu`, `memory` or `disk`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::AcceleratorModel => "accelerator_model",
            Self::AcceleratorCount => "accelerator_count",
            Self::Architecture => "architecture",
            Self::Cpu => "cpu",
            Self::Memory => "memory",
            Self::Disk => "disk",
        }
    }
}

impl fmt::Display for HelloDimension {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(self.name())
    }
}

/// What [`verify_hello`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The hello meets the offer in every dimension.
    Match,
    /// The dimensions the hello failed, each once, in the order of [`HelloDimension`].
    Mismatch(Vec<HelloDimension>),
}

/// Whether `have` is at least `offered` less `tolerance_percent` percent of it. The bound is
/// exact: `have * 100 >= offered * (100 - tolerance)`, in 128-bit integers.
fn within(have: u64, offered: u64, tolerance_percent: u8) -> bool {
    let kept = 100_u128.saturating_sub(u128::from(tolerance_percent));
    u128::from(have).saturating_mul(100) >= u128::from(offered).saturating_mul(kept)
}

/// Whether `hello`'s accelerator is of the model `offer` names: the offer's class read as a
/// model through the alias table and met by the hello's accelerator. An offer with no class,
/// or a class that is not a model name, names none, and any accelerator (or none) meets it.
fn same_model(offer: &StoredOffer, hello: &Offer) -> bool {
    let named = offer
        .gpu_class
        .as_deref()
        .and_then(|class| Model::parse(class).ok());
    named.is_none_or(|model| {
        let wanted = AcceleratorNeed {
            model: Some(model),
            ..AcceleratorNeed::default()
        };
        wanted.met_by(hello.accelerator.as_ref()).is_ok()
    })
}

/// Compares the shape `hello` announced with the `offer` its machine was acquired for, by
/// the rule of design 5.6.1, step 6, with CPU, memory and disk allowed to be
/// `tolerance_percent` below the offer (a value above 100 is read as 100). See the module
/// documentation for each dimension.
#[must_use]
pub fn verify_hello(offer: &StoredOffer, hello: &Offer, tolerance_percent: u8) -> Verdict {
    let count = hello.accelerator.as_ref().map_or(0, |own| own.count);
    let architecture = [
        hello.arch.is_empty(),
        offer.arch.is_empty(),
        normalise_arch(&offer.arch) == normalise_arch(&hello.arch),
    ];
    let disk = [
        hello.disk_bytes == 0,
        within(hello.disk_bytes, offer.disk_bytes, tolerance_percent),
    ];
    let checks = [
        (HelloDimension::AcceleratorModel, same_model(offer, hello)),
        (HelloDimension::AcceleratorCount, count >= offer.gpus),
        (
            HelloDimension::Architecture,
            architecture.into_iter().any(|ok| ok),
        ),
        (
            HelloDimension::Cpu,
            within(hello.cpu_millis, offer.cpu_millis, tolerance_percent),
        ),
        (
            HelloDimension::Memory,
            within(hello.memory_bytes, offer.memory_bytes, tolerance_percent),
        ),
        (HelloDimension::Disk, disk.into_iter().any(|ok| ok)),
    ];
    let failed: Vec<HelloDimension> = checks
        .into_iter()
        .filter(|(_, met)| !met)
        .map(|(dimension, _)| dimension)
        .collect();
    match failed.is_empty() {
        true => Verdict::Match,
        false => Verdict::Mismatch(failed),
    }
}

#[cfg(test)]
mod tests {
    use js_elastic::PriceUnit;
    use js_types::{ExecMode, accelerator_of};

    use super::*;

    /// An offer of eight devices spelt `class`, with ample CPU and memory.
    fn offer(class: &str) -> Offer {
        let model = Model::parse(class).unwrap();
        Offer {
            id: "gpu_8x".to_owned(),
            zone: None,
            cpu_millis: 8000,
            memory_bytes: 64 << 30,
            disk_bytes: 0,
            accelerator: Some(accelerator_of(&model, 8, None)),
            os: "linux".to_owned(),
            arch: "x86_64".to_owned(),
            isa_level: None,
            exec: ExecMode::Container,
            price_cents_per_hour: Some(1),
            price_unit: PriceUnit::UsCents,
            available: true,
            purchase: None,
            quota: None,
            block: None,
        }
    }

    fn matches(stored: &StoredOffer, hello: &Offer) -> bool {
        verify_hello(stored, hello, 10) == Verdict::Match
    }

    /// Asserts that a host of another variant of the offer's family does not match a variant
    /// offer (a 40 GB host of an 80 GB offer, and the reverse) and does match when it
    /// reports that variant; that a host of any variant matches a family offer; and that a
    /// host of another family matches neither. Catches a check that compares families
    /// only, which keeps a machine that came up smaller than what was paid for.
    #[test]
    fn a_host_of_another_variant_does_not_match_a_variant_offer() {
        let (a80, a40) = ("nvidia-a100-sxm4-80gb", "nvidia-a100-sxm4-40gb");
        let stored = |class| StoredOffer::from_offer(&offer(class));
        assert!(!matches(&stored(a80), &offer(a40)));
        assert!(!matches(&stored(a40), &offer(a80)));
        assert!(matches(&stored(a80), &offer(a80)));
        assert!(matches(&stored("nvidia-a100"), &offer(a40)));
        assert!(matches(&stored("nvidia-a100"), &offer(a80)));
        assert_eq!(
            verify_hello(&stored("nvidia-a100"), &offer("nvidia-h100"), 10),
            Verdict::Mismatch(vec![HelloDimension::AcceleratorModel])
        );
    }

    /// Asserts each dimension's name as the records file and the status view spell it, and
    /// that `Display` writes the same. Catches a renamed spelling that the records file of
    /// an earlier release would no longer read.
    #[test]
    fn each_dimension_has_its_one_spelling() {
        let all = [
            (HelloDimension::AcceleratorModel, "accelerator_model"),
            (HelloDimension::AcceleratorCount, "accelerator_count"),
            (HelloDimension::Architecture, "architecture"),
            (HelloDimension::Cpu, "cpu"),
            (HelloDimension::Memory, "memory"),
            (HelloDimension::Disk, "disk"),
        ];
        for (dimension, name) in all {
            assert_eq!(dimension.name(), name);
            assert_eq!(dimension.to_string(), name);
            let json = serde_json::to_string(&dimension).unwrap();
            assert_eq!(json, format!("\"{name}\""));
            let back: HelloDimension = serde_json::from_str(&json).unwrap();
            assert_eq!(back, dimension);
        }
    }
}
