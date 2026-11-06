//! One machine type in one zone as offers: one per provisioning model the configuration
//! lists and the type is sold on, for the type with its built-in accelerators, and for an
//! N1 type also one per accelerator the zone offers to attach and number of it the N1
//! table allows.
//!
//! Which types have which accelerators: the accelerator-optimized types (A2, A3, A4, A4X,
//! G2, G4) list theirs in `accelerators`, which states them; an N1 type has none built in
//! and takes the T4, P4 or V100 attached, in a zone whose `acceleratorTypes` list offers
//! it; every other type has none.

use js_caps::Arch;
use js_elastic::{Offer, PriceUnit};
use js_types::ExecMode;

use crate::api::PurchaseKind;
use crate::gpu::{self, Translated};
use crate::model::{AttachedAccelerator, MachineTypeInfo};
use crate::price::Prices;
use crate::quota::RegionQuotas;

/// Bytes in a MiB.
const MIB: u64 = 1 << 20;

/// The operating system of every offer: the catalog offers Linux machines.
const OS: &str = "linux";

/// The architecture of a series whose machine types do not state one: a machine type name
/// prefix and the architecture, from the machine families resource page (its architecture
/// row) and the Arm VMs page. A series with no row and no stated architecture has no offer.
pub(crate) const ARCH_ROWS: [(&str, Arch); 16] = [
    // C4A: Google Axion, Arm ("c4a-standard-4-lssd", machine families resource).
    ("c4a-", Arch::Arm64),
    // N4A: Google Axion, Arm (Arm VMs on Compute).
    ("n4a-", Arch::Arm64),
    // Tau T2A: Ampere Altra, Arm (Arm VMs on Compute).
    ("t2a-", Arch::Arm64),
    // A4X and A4X Max: NVIDIA Grace, Arm ("a4x-highgpu-4g", "a4x-maxgpu-4g-metal", Arm VMs
    // on Compute).
    ("a4x-", Arch::Arm64),
    // E2: x86 ("e2-micro", machine families resource).
    ("e2-", Arch::X86_64),
    // N1: x86 (machine families resource; "n1-standard" in GPU machine types).
    ("n1-", Arch::X86_64),
    // N2: x86 ("n2-standard-4", machine families resource).
    ("n2-", Arch::X86_64),
    // N2D: x86 (machine families resource).
    ("n2d-", Arch::X86_64),
    // C3: x86 ("c3-standard-22", machine families resource).
    ("c3-", Arch::X86_64),
    // C3D: x86 ("c3d-highmem-360-lssd", machine families resource).
    ("c3d-", Arch::X86_64),
    // Z3: x86 ("z3-highmem-22-standardlssd", machine families resource).
    ("z3-", Arch::X86_64),
    // A2: x86 (machine families resource).
    ("a2-", Arch::X86_64),
    // A3: x86 ("a3-edgegpu-8g-nolssd", machine families resource).
    ("a3-", Arch::X86_64),
    // A4: x86 (machine families resource; "a4-highgpu-8g").
    ("a4-", Arch::X86_64),
    // G2: x86 (machine families resource).
    ("g2-", Arch::X86_64),
    // G4: x86 (machine families resource).
    ("g4-", Arch::X86_64),
];

/// The provisioning models a series is sold on, where it is not sold on both: a machine
/// type name prefix and the models. From the consumption options of the
/// accelerator-optimized machine family and creating a VM with attached GPUs, and the GPU
/// quota table ("Standard quota is not available for A4X Max, A4X, A4 and A3 Ultra"). The
/// reservation and scheduled models these types are also sold on (capacity blocks,
/// calendar mode, flex-start) are not offered by this catalog.
pub(crate) const SOLD_ROWS: [(&str, &[PurchaseKind]); 6] = [
    // A4X and A4X Max: future reservations for capacity blocks only; "A4X instances don't
    // support the following: ... Spot VMs".
    ("a4x-", &[]),
    // A4: Spot, flex-start, calendar mode and capacity blocks; no standard quota.
    ("a4-", &[PurchaseKind::Spot]),
    // A3 Ultra: Spot, flex-start, calendar mode and capacity blocks; no standard quota.
    ("a3-ultragpu-", &[PurchaseKind::Spot]),
    // A3 High with 1 GPU: "you must create instances by using Spot VMs or Flex-start VMs".
    ("a3-highgpu-1g", &[PurchaseKind::Spot]),
    // A3 High with 2 GPUs: Spot or flex-start, as with 1.
    ("a3-highgpu-2g", &[PurchaseKind::Spot]),
    // A3 High with 4 GPUs: Spot or flex-start, as with 1.
    ("a3-highgpu-4g", &[PurchaseKind::Spot]),
];

/// Whether a machine type named `machine` is sold as `kind`.
pub(crate) fn sold(machine: &str, kind: PurchaseKind) -> bool {
    SOLD_ROWS
        .iter()
        .find(|(prefix, _)| machine.starts_with(prefix))
        .is_none_or(|(_, kinds)| kinds.contains(&kind))
}

/// Why a machine has no offer at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Dropped {
    /// Its architecture is not stated and no row of the table gives it.
    Arch,
    /// Its accelerator type name has no letter or digit to spell a model with.
    Unnamed,
}

/// What a machine gives, whatever provisioning model: an offer with no id, price,
/// purchase option or quota yet, and the pass-through spelling of its accelerator when
/// the translation table does not know it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Machine {
    pub(crate) offer: Offer,
    pub(crate) unknown_accelerator: Option<String>,
}

/// The architecture of `info`: its stated `architecture` (`X86_64`, `ARM64`) as `js-caps`
/// reads it in lower case, or else the row of the table whose prefix starts its name.
fn arch(info: &MachineTypeInfo) -> Option<Arch> {
    let stated = info
        .arch
        .as_ref()
        .and_then(|name| name.0.to_ascii_lowercase().parse::<Arch>().ok());
    stated.or_else(|| {
        ARCH_ROWS
            .iter()
            .find(|(prefix, _)| info.name.as_str().starts_with(prefix))
            .map(|(_, arch)| *arch)
    })
}

/// What `info` gives with `accelerator` (its built-in accelerators, or ones attached to
/// it): CPU (`vcpus` whole cores), memory, the accelerators from [`gpu::translate`],
/// Linux, the architecture, containers, in the zone of `info`, available.
///
/// The disk is `0`: a Compute Engine machine type has no disk of its own, and the boot
/// disk a launch attaches is sized by the launch side to the pool's shape.
///
/// # Errors
///
/// [`Dropped`] when the architecture cannot be determined or the accelerator has no name.
pub(crate) fn machine(
    info: &MachineTypeInfo,
    accelerator: Option<&AttachedAccelerator>,
) -> Result<Machine, Dropped> {
    let arch = arch(info).ok_or(Dropped::Arch)?;
    let translated =
        accelerator.map(|attached| gpu::translate(&attached.type_name, attached.count));
    let (accelerator, unknown_accelerator) = match translated {
        None => (None, None),
        Some(Translated::Known(accelerator)) => (Some(accelerator), None),
        Some(Translated::Unknown(accelerator)) => {
            let spelling = accelerator.model.to_string();
            (Some(accelerator), Some(spelling))
        }
        Some(Translated::Unnamed) => return Err(Dropped::Unnamed),
    };
    let offer = Offer {
        id: String::new(),
        zone: Some(info.zone.as_str().to_owned()),
        cpu_millis: u64::from(info.vcpus) * 1000,
        memory_bytes: info.memory_mib.saturating_mul(MIB),
        disk_bytes: 0,
        accelerator,
        os: OS.to_owned(),
        arch: arch.name().to_owned(),
        // A machine type does not say which ISA level its CPU reaches, so the offer states
        // none and fits no demand that asks for a level.
        isa_level: None,
        exec: ExecMode::Container,
        price_cents_per_hour: None,
        price_unit: PriceUnit::UsCents,
        available: true,
        purchase: None,
        quota: None,
        block: None,
    };
    Ok(Machine {
        offer,
        unknown_accelerator,
    })
}

/// The id of an offer of `info` with `attached` accelerators bought as `kind`: the type
/// and the provisioning model (`a3-megagpu-8g/spot`), and for attached accelerators the
/// type, `+`, the accelerator, `x` and the number (`n1-standard-8+nvidia-tesla-t4x2/spot`).
/// The zone is the offer's own field.
fn offer_id(
    info: &MachineTypeInfo,
    attached: Option<&AttachedAccelerator>,
    kind: PurchaseKind,
) -> String {
    match attached {
        Some(attached) => format!(
            "{}+{}x{}/{}",
            info.name,
            attached.type_name,
            attached.count,
            kind.name()
        ),
        None => format!("{}/{}", info.name, kind.name()),
    }
}

/// The offers of `machine`, the machine type `info` with `attached` accelerators (or its
/// built-in ones), for the provisioning models `purchase` lists in its order and the type
/// is sold on, and for no other: each priced from `prices` (`None` when a part has no SKU,
/// so the offer is listed and unpriced) and carrying the quota of `quotas` it counts
/// against (`None` when the quotas could not be read).
pub(crate) fn offers_for(
    info: &MachineTypeInfo,
    attached: Option<&AttachedAccelerator>,
    machine: &Machine,
    prices: &Prices,
    quotas: Option<&RegionQuotas>,
    purchase: &[PurchaseKind],
) -> Vec<Offer> {
    let accelerators = attached.or(info.accelerators.first());
    purchase
        .iter()
        .filter(|kind| sold(info.name.as_str(), **kind))
        .map(|kind| Offer {
            id: offer_id(info, attached, *kind),
            price_cents_per_hour: prices.hourly(
                &info.name,
                info.vcpus,
                info.memory_mib,
                accelerators.map(|a| (&a.type_name, a.count)),
                *kind,
            ),
            purchase: Some(kind.purchase()),
            quota: quotas.and_then(|quotas| {
                quotas.quota_for(&info.name, accelerators.map(|a| &a.type_name), *kind)
            }),
            ..machine.offer.clone()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts that every row of the architecture table is reached by a type that starts
    /// with its prefix and states no architecture, and that a stated architecture wins
    /// over the table; and that every row of the provisioning table holds only for the
    /// types it starts. Catches a row that maps to the other architecture, or a model
    /// table that leaks onto another series.
    #[test]
    fn each_table_row_is_reached_by_its_prefix() {
        let info = |name: &str, stated: Option<&str>| MachineTypeInfo {
            name: crate::model::MachineTypeName(name.to_owned()),
            zone: crate::model::Zone::new("zone-one-a").unwrap(),
            vcpus: 1,
            memory_mib: 1024,
            accelerators: Vec::new(),
            arch: stated.map(|name| crate::model::ArchName(name.to_owned())),
        };
        for (prefix, row) in ARCH_ROWS {
            assert_eq!(
                arch(&info(&format!("{prefix}standard-9"), None)),
                Some(row),
                "{prefix}"
            );
        }
        assert_eq!(
            arch(&info("a4x-highgpu-4g", Some("X86_64"))),
            Some(Arch::X86_64)
        );
        assert_eq!(
            arch(&info("q9-standard-9", Some("ARCHITECTURE_UNSPECIFIED"))),
            None
        );
        for (prefix, kinds) in SOLD_ROWS {
            for kind in [PurchaseKind::OnDemand, PurchaseKind::Spot] {
                assert_eq!(sold(prefix, kind), kinds.contains(&kind), "{prefix}");
            }
        }
        assert!(sold("a3-highgpu-8g", PurchaseKind::OnDemand));
        assert!(sold("a3-megagpu-8g", PurchaseKind::OnDemand));
        assert!(sold("n1-standard-8", PurchaseKind::Spot));
    }
}
