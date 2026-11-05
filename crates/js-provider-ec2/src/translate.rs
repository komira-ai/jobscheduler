//! One instance type as offers: one per (availability zone, purchase option) for
//! on-demand and spot, and one per Capacity Block offering.

use std::collections::{BTreeMap, BTreeSet};

use js_caps::Arch;
use js_elastic::{BlockTerms, Offer, PriceUnit, Quota};
use js_types::ExecMode;

use crate::api::PurchaseKind;
use crate::gpu::{self, Gpus};
use crate::model::{BlockOffering, InstanceType, InstanceTypeInfo, Zone};
use crate::price;

/// Bytes in a MiB.
const MIB: u64 = 1 << 20;

/// Bytes in a GB, the unit of `InstanceStorageInfo.TotalSizeInGB`.
const GB: u64 = 1_000_000_000;

/// The operating system of every offer: the catalog offers Linux machines.
const OS: &str = "linux";

/// Why a type has no offer at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Dropped {
    /// None of its architectures is one the scheduler runs (an `i386`-only or a Mac type).
    Arch,
    /// Its GPU has no name to spell a model with.
    Unnamed,
}

/// What a type gives, whatever zone and purchase option: an offer with no zone, price,
/// purchase option or quota yet, and the pass-through spelling of its GPU when the
/// translation table does not know it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Machine {
    pub(crate) offer: Offer,
    pub(crate) unknown_gpu: Option<String>,
}

/// Where and how a type is sold, read from the cloud's answers.
#[derive(Debug, Default)]
pub(crate) struct Sale<'a> {
    /// The zones whose offering lists the type, sorted.
    pub(crate) zones: BTreeSet<&'a Zone>,
    /// The on-demand price in cents per hour, if the type has one.
    pub(crate) on_demand: Option<u64>,
    /// The spot price of each zone that has one, in cents per hour.
    pub(crate) spot: BTreeMap<&'a Zone, u64>,
    /// The quota of each purchase option the account has one for.
    pub(crate) quota: BTreeMap<PurchaseKind, Quota>,
    /// The Capacity Block offerings of the type with their upfront price in cents, sorted
    /// by zone and offering id.
    pub(crate) blocks: Vec<(&'a BlockOffering, u64)>,
}

/// The architecture of a type: the first of `ProcessorInfo.SupportedArchitectures` that
/// the scheduler runs, read by `js-caps` (so `aarch64` would be `arm64`). An EC2 type lists
/// one of `x86_64` and `arm64` (beside `i386` on some older types); if one ever listed
/// both, the first listed is stated.
fn arch(info: &InstanceTypeInfo) -> Option<Arch> {
    info.architectures
        .iter()
        .find_map(|name| name.0.parse::<Arch>().ok())
}

/// What `info` gives in any zone: CPU (`vcpus` whole cores), memory, the instance store as
/// disk, the accelerators from [`gpu::translate`], Linux, the architecture, containers.
///
/// The disk is the type's instance store; a type with none states `0`, because the root
/// volume a launch attaches is sized by the launch side to the pool's shape, not by the
/// type.
///
/// # Errors
///
/// [`Dropped`] when the type has no architecture the scheduler runs or its GPU no name.
pub(crate) fn machine(info: &InstanceTypeInfo) -> Result<Machine, Dropped> {
    let arch = arch(info).ok_or(Dropped::Arch)?;
    let (accelerator, unknown_gpu) = match gpu::translate(&info.gpus) {
        Gpus::None => (None, None),
        Gpus::Known(accelerator) => (Some(accelerator), None),
        Gpus::Unknown(accelerator) => {
            let spelling = accelerator.model.to_string();
            (Some(accelerator), Some(spelling))
        }
        Gpus::Unnamed => return Err(Dropped::Unnamed),
    };
    let offer = Offer {
        id: info.name.to_string(),
        zone: None,
        cpu_millis: u64::from(info.vcpus) * 1000,
        memory_bytes: info.memory_mib.saturating_mul(MIB),
        disk_bytes: info.instance_storage_gb.unwrap_or(0).saturating_mul(GB),
        accelerator,
        os: OS.to_owned(),
        arch: arch.name().to_owned(),
        // DescribeInstanceTypes does not say which ISA level a type's CPU reaches, so the
        // offer states none and fits no demand that asks for a level.
        isa_level: None,
        exec: ExecMode::Container,
        price_cents_per_hour: None,
        price_unit: PriceUnit::UsCents,
        available: true,
        purchase: None,
        quota: None,
        block: None,
    };
    Ok(Machine { offer, unknown_gpu })
}

/// The id of an offer of `instance_type` bought as `kind`: `type/option`
/// (`p5.48xlarge/spot`), and for a Capacity Block `type/capacity-block/offering`. The zone
/// is the offer's own field.
fn offer_id(instance_type: &InstanceType, kind: PurchaseKind, offering: Option<&str>) -> String {
    match offering {
        Some(offering) => format!("{instance_type}/{}/{offering}", kind.name()),
        None => format!("{instance_type}/{}", kind.name()),
    }
}

/// The offers of `machine` for the options `purchase` lists, and for no other: for
/// on-demand and spot, one per zone of `sale`, available, priced from `sale` (`None` when
/// the answer has no price for it, so the offer is listed and unpriced) and carrying the
/// quota of that option; for capacity blocks, one per offering, priced at its upfront fee
/// over its instance hours and carrying its terms and no quota (instances in a Capacity
/// Block do not count against the on-demand limits). Sorted by zone, then the order of
/// `purchase`, then the offering.
pub(crate) fn offers_for(
    machine: &Machine,
    instance_type: &InstanceType,
    sale: &Sale<'_>,
    purchase: &[PurchaseKind],
) -> Vec<Offer> {
    let sold = |kind: PurchaseKind, zone: &Zone, price: Option<u64>| Offer {
        id: offer_id(instance_type, kind, None),
        zone: Some(zone.as_str().to_owned()),
        price_cents_per_hour: price,
        purchase: Some(kind.purchase()),
        quota: sale.quota.get(&kind).copied(),
        ..machine.offer.clone()
    };
    let mut offers = Vec::new();
    for kind in purchase {
        match kind {
            PurchaseKind::OnDemand => offers.extend(
                sale.zones
                    .iter()
                    .map(|zone| sold(*kind, *zone, sale.on_demand)),
            ),
            PurchaseKind::Spot => offers.extend(
                sale.zones
                    .iter()
                    .map(|zone| sold(*kind, *zone, sale.spot.get(zone).copied())),
            ),
            PurchaseKind::CapacityBlock => {
                offers.extend(sale.blocks.iter().map(|(offering, upfront)| Offer {
                    id: offer_id(instance_type, *kind, Some(&offering.id)),
                    zone: Some(offering.zone.as_str().to_owned()),
                    price_cents_per_hour: price::hourly_share(
                        *upfront,
                        offering.instance_count,
                        offering.duration_minutes,
                    ),
                    purchase: Some(kind.purchase()),
                    block: Some(BlockTerms {
                        offering: offering.id.clone(),
                        start_unix_seconds: offering.start_unix_seconds,
                        duration_minutes: offering.duration_minutes,
                        upfront_cents: *upfront,
                        instances: offering.instance_count,
                    }),
                    ..machine.offer.clone()
                }));
            }
        }
    }
    // A stable sort: within a zone the order of `purchase`, and the offerings in theirs.
    offers.sort_by(|a, b| a.zone.cmp(&b.zone));
    offers
}
