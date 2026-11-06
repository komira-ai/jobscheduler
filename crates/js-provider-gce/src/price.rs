//! Prices in US cents per hour, from the Cloud Billing catalog's SKUs, with integer
//! arithmetic on the `units` and `nanos` of each unit price: no float, so no rounding
//! drift. A fraction of a cent rounds up, so an offer never states a price below what the
//! SKUs add up to (and a price of a tenth of a cent is 1, never a free 0). A unit price
//! that cannot be read is an error, never zero.
//!
//! A machine is priced by parts, as Compute Engine bills the series the table lists: its
//! vCPUs at the series' core price, its memory at the series' memory price per GiB, and
//! each accelerator, built in or attached, at the accelerator's price. The standard
//! provisioning model reads the `OnDemand` SKUs and Spot the `Preemptible` ones. A series
//! the table does not list is not priced: its offers are listed with no price, which the
//! engine drops from a launch and admission still sees. Left out on purpose are the series
//! billed for a bundled Local SSD beside their parts (A2 Ultra, A3, A4: "Google Cloud bills
//! A3 instances for their attached GPUs, predefined vCPUs, memory, and bundled Local SSD
//! disks", accelerator-optimized machine family), which the parts would price too low, and
//! the series whose billing this crate has not read from Google's documentation.

use std::collections::BTreeMap;

use crate::api::PurchaseKind;
use crate::error::ApiError;
use crate::model::{
    AcceleratorTypeName, MachineTypeName, Money, PricedItem, SeriesName, SkuPrice, UsageType,
};

/// The only currency the catalog reads: offers are priced in US cents.
const USD: &str = "USD";

/// Billionths of a unit in one unit (`Money.nanos`).
const NANOS: u128 = 1_000_000_000;

/// Billionths of a dollar in one cent.
const NANOS_PER_CENT: u128 = 10_000_000;

/// MiB in a GiB: memory is priced per GiB-hour and stated in MiB.
const MIB_PER_GIB: u128 = 1024;

/// The table: a machine type name prefix and the billing series whose core and memory
/// SKUs price it.
pub(crate) const SERIES_ROWS: [(&str, &str); 4] = [
    // N1: vCPUs and memory are priced per series, and GPUs are attached by number ("N1
    // machine types don't come with a set number of attached GPUs", GPU machine types),
    // each priced on its own.
    ("n1-", "n1"),
    // A2 Standard (`a2-highgpu-*`): "Google Cloud bills A2 instances for their attached
    // GPUs, predefined vCPUs, memory" (accelerator-optimized machine family); A2 Standard
    // bundles no Local SSD, which is billed on its own.
    ("a2-highgpu-", "a2"),
    // A2 Standard with 16 GPUs (`a2-megagpu-16g`), billed as the other A2 Standard types.
    ("a2-megagpu-", "a2"),
    // G4: billed for its GPUs, vCPUs and memory, with its Titanium SSD billed apart from
    // the machine type (accelerator-optimized machine family).
    ("g4-", "g4"),
];

/// The billing series of `machine`, if a row's prefix starts its name.
pub(crate) fn series_of(machine: &MachineTypeName) -> Option<&'static str> {
    SERIES_ROWS
        .iter()
        .find(|(prefix, _)| machine.as_str().starts_with(prefix))
        .map(|(_, series)| *series)
}

/// `money` in billionths of a US dollar.
///
/// # Errors
///
/// [`ApiError::Unreadable`] when it is not in US dollars, its `units` are not the digits
/// of a whole number that fits 64 bits (a sign is refused: a price is never negative), or
/// its `nanos` are not 0 to 999,999,999.
pub(crate) fn nanos(money: &Money) -> Result<u128, ApiError> {
    let unreadable = || {
        ApiError::Unreadable(format!(
            "{} units and {} nanos of {:?} is not a price in US dollars",
            money.units, money.nanos, money.currency
        ))
    };
    let readable = [
        money.currency == USD,
        !money.units.is_empty(),
        money.units.bytes().all(|b| b.is_ascii_digit()),
        (0..1_000_000_000).contains(&money.nanos),
    ];
    if !readable.into_iter().all(|ok| ok) {
        return Err(unreadable());
    }
    let units: u64 = money.units.parse().map_err(|_| unreadable())?;
    // `nanos` is 0 to 999,999,999 here, so the conversion cannot fail.
    let fraction = u128::try_from(money.nanos).unwrap_or(0);
    Ok(u128::from(units) * NANOS + fraction)
}

/// The usage type whose SKUs price `kind`.
fn usage_of(kind: PurchaseKind) -> UsageType {
    match kind {
        PurchaseKind::OnDemand => UsageType::OnDemand,
        PurchaseKind::Spot => UsageType::Preemptible,
    }
}

/// The unit prices of one region, in billionths of a dollar, by what they price and how.
#[derive(Debug, Default)]
pub(crate) struct Prices(BTreeMap<(PricedItem, UsageType), u128>);

impl Prices {
    /// The prices of `skus`. An item priced twice for one usage type (it should not be)
    /// takes the higher price, so the answer's order never matters.
    ///
    /// # Errors
    ///
    /// [`ApiError::Unreadable`] naming the first unit price that cannot be read.
    pub(crate) fn read(skus: &[SkuPrice]) -> Result<Self, ApiError> {
        let mut table = BTreeMap::new();
        for sku in skus {
            let price = nanos(&sku.unit_price)?;
            let entry = table.entry((sku.item.clone(), sku.usage)).or_insert(price);
            *entry = (*entry).max(price);
        }
        Ok(Self(table))
    }

    fn get(&self, item: PricedItem, usage: UsageType) -> Option<u128> {
        self.0.get(&(item, usage)).copied()
    }

    /// The price in cents per hour of a machine of the type `machine` with `vcpus` vCPUs,
    /// `memory_mib` MiB of memory and `accelerators` (a type and how many), bought as
    /// `kind`, rounded up to a whole cent. `None` when the type's series has no row, when
    /// a part has no SKU, or when the price does not fit 64 bits.
    pub(crate) fn hourly(
        &self,
        machine: &MachineTypeName,
        vcpus: u32,
        memory_mib: u64,
        accelerators: Option<(&AcceleratorTypeName, u32)>,
        kind: PurchaseKind,
    ) -> Option<u64> {
        let usage = usage_of(kind);
        let series = SeriesName(series_of(machine)?.to_owned());
        let core = self.get(PricedItem::Cores(series.clone()), usage)?;
        let memory = self.get(PricedItem::Memory(series), usage)?;
        let accelerator = match accelerators {
            Some((name, count)) => {
                self.get(PricedItem::Accelerator(name.clone()), usage)? * u128::from(count)
            }
            None => 0,
        };
        // Everything in billionths of a dollar per hour. A unit price is below 2^64 x 10^9
        // (below 2^94) and a count below 2^32, so each product is below 2^126 and their sum
        // fits 128 bits; the memory is added times 1024, so that memory priced per GiB and
        // stated in MiB stays whole, and those steps can overflow.
        let parts = core * u128::from(vcpus) + accelerator;
        let total = parts
            .checked_mul(MIB_PER_GIB)?
            .checked_add(memory.checked_mul(u128::from(memory_mib))?)?;
        u64::try_from(total.div_ceil(MIB_PER_GIB * NANOS_PER_CENT)).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn money(units: &str, nanos: i32) -> Money {
        Money {
            currency: USD.to_owned(),
            units: units.to_owned(),
            nanos,
        }
    }

    fn sku(item: PricedItem, usage: UsageType, units: &str, nanos: i32) -> SkuPrice {
        SkuPrice {
            item,
            usage,
            unit_price: money(units, nanos),
        }
    }

    fn series(name: &str) -> SeriesName {
        SeriesName(name.to_owned())
    }

    /// Asserts that `units` and `nanos` become billionths of a dollar exactly, from zero to
    /// the largest that fits, and that what is not a price in US dollars is unreadable,
    /// with the whole value in the error: another currency, empty or signed or spaced or
    /// fractional units, units beyond 64 bits, and nanos below zero or of a whole unit.
    /// Catches float drift, a sign or fraction read, and an unreadable price read as zero.
    #[test]
    fn units_and_nanos_become_billionths_of_a_dollar() {
        assert_eq!(nanos(&money("0", 0)), Ok(0));
        assert_eq!(nanos(&money("0", 1)), Ok(1));
        assert_eq!(nanos(&money("2", 500_000_000)), Ok(2_500_000_000));
        assert_eq!(nanos(&money("0", 999_999_999)), Ok(999_999_999));
        assert_eq!(
            nanos(&money("18446744073709551615", 999_999_999)),
            Ok(u128::from(u64::MAX) * NANOS + 999_999_999)
        );
        let euro = Money {
            currency: "EUR".to_owned(),
            ..money("1", 0)
        };
        assert_eq!(
            nanos(&euro),
            Err(ApiError::Unreadable(
                "1 units and 0 nanos of \"EUR\" is not a price in US dollars".to_owned()
            ))
        );
        for (units, fraction) in [
            ("", 0),
            ("-1", 0),
            ("+1", 0),
            (" 1", 0),
            ("1.5", 0),
            ("free", 0),
            ("18446744073709551616", 0),
            ("0", -1),
            ("1", -500_000_000),
            ("0", 1_000_000_000),
        ] {
            assert!(
                matches!(nanos(&money(units, fraction)), Err(ApiError::Unreadable(_))),
                "{units:?} {fraction}"
            );
        }
    }

    /// Asserts that a machine is priced by its parts, rounded up to a cent: four vCPUs at
    /// $0.031611, 15 GiB at $0.004237 and one T4 at $0.35 make $0.539999, 54 cents; that
    /// Spot reads the preemptible SKUs; that memory of a part of a GiB is priced in MiB;
    /// that a type with no series row, a part with no SKU, and a price beyond 64 bits are
    /// not priced; and that an item priced twice takes the higher price in either order.
    /// Catches truncation, a swapped usage type, and an unpriced part read as free.
    #[test]
    fn a_machine_is_priced_by_its_parts() {
        let t4 = AcceleratorTypeName("nvidia-tesla-t4".to_owned());
        let n1 = MachineTypeName("n1-standard-4".to_owned());
        let on_demand = [
            sku(
                PricedItem::Cores(series("n1")),
                UsageType::OnDemand,
                "0",
                31_611_000,
            ),
            sku(
                PricedItem::Memory(series("n1")),
                UsageType::OnDemand,
                "0",
                4_237_000,
            ),
            sku(
                PricedItem::Accelerator(t4.clone()),
                UsageType::OnDemand,
                "0",
                350_000_000,
            ),
            sku(
                PricedItem::Cores(series("n1")),
                UsageType::Preemptible,
                "0",
                6_655_000,
            ),
            sku(
                PricedItem::Memory(series("n1")),
                UsageType::Preemptible,
                "0",
                892_000,
            ),
        ];
        let prices = Prices::read(&on_demand).unwrap();
        let hourly = |vcpus, memory_mib, accelerators, kind| {
            prices.hourly(&n1, vcpus, memory_mib, accelerators, kind)
        };
        assert_eq!(
            hourly(4, 15 * 1024, Some((&t4, 1)), PurchaseKind::OnDemand),
            Some(54)
        );
        assert_eq!(hourly(4, 15 * 1024, None, PurchaseKind::OnDemand), Some(19));
        // 4 x 0.006655 + 15 x 0.000892 = 0.0400; no Spot T4 SKU, so with one it is unpriced.
        assert_eq!(hourly(4, 15 * 1024, None, PurchaseKind::Spot), Some(4));
        assert_eq!(
            hourly(4, 15 * 1024, Some((&t4, 1)), PurchaseKind::Spot),
            None
        );
        // 512 MiB at $0.004237 per GiB-hour is a fraction of a cent: 1.
        assert_eq!(hourly(0, 512, None, PurchaseKind::OnDemand), Some(1));
        assert_eq!(hourly(0, 0, None, PurchaseKind::OnDemand), Some(0));
        let e2 = MachineTypeName("e2-standard-4".to_owned());
        assert_eq!(
            prices.hourly(&e2, 4, 1024, None, PurchaseKind::OnDemand),
            None
        );
        let most = "18446744073709551615";
        let huge = Prices::read(&[
            sku(
                PricedItem::Cores(series("n1")),
                UsageType::OnDemand,
                most,
                999_999_999,
            ),
            sku(
                PricedItem::Memory(series("n1")),
                UsageType::OnDemand,
                most,
                999_999_999,
            ),
            sku(
                PricedItem::Accelerator(t4.clone()),
                UsageType::OnDemand,
                most,
                999_999_999,
            ),
        ])
        .unwrap();
        let price = |vcpus, memory_mib, accelerators| {
            huge.hourly(&n1, vcpus, memory_mib, accelerators, PurchaseKind::OnDemand)
        };
        assert_eq!(price(4, 0, None), None, "cents beyond 64 bits");
        assert_eq!(
            price(u32::MAX, 0, Some((&t4, u32::MAX))),
            None,
            "parts times 1024"
        );
        assert_eq!(price(0, u64::MAX, None), None, "memory beyond 128 bits");
        assert_eq!(
            price(12_000_000, 12_000_000_000, None),
            None,
            "their sum beyond 128 bits"
        );
        for order in [["1", "2"], ["2", "1"]] {
            let skus = order.map(|units| {
                sku(
                    PricedItem::Cores(series("g4")),
                    UsageType::OnDemand,
                    units,
                    0,
                )
            });
            let mut skus = skus.to_vec();
            skus.push(sku(
                PricedItem::Memory(series("g4")),
                UsageType::OnDemand,
                "0",
                0,
            ));
            let twice = Prices::read(&skus).unwrap();
            let g4 = MachineTypeName("g4-standard-48".to_owned());
            assert_eq!(
                twice.hourly(&g4, 1, 0, None, PurchaseKind::OnDemand),
                Some(200)
            );
        }
        assert!(
            Prices::read(&[sku(
                PricedItem::Cores(series("n1")),
                UsageType::OnDemand,
                "x",
                0
            )])
            .is_err()
        );
    }

    /// Asserts that every row of the series table is reached by a machine type that starts
    /// with its prefix, and that a type no row starts (A2 Ultra, A3, an E2) has none.
    #[test]
    fn each_priced_series_is_found_by_its_prefix() {
        for (prefix, row) in SERIES_ROWS {
            let name = MachineTypeName(format!("{prefix}9"));
            assert_eq!(series_of(&name), Some(row), "{prefix}");
        }
        for name in ["a2-ultragpu-8g", "a3-highgpu-8g", "e2-standard-2", "n1"] {
            assert_eq!(series_of(&MachineTypeName(name.to_owned())), None, "{name}");
        }
    }
}
