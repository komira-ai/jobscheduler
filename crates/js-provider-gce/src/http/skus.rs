//! The Cloud Billing reads of the SKUs of the Compute Engine service: which SKUs the
//! catalog keeps, what each prices (read from its description) and its unit price. A SKU
//! that is not read is left out, never an error; a SKU that is read and whose price cannot
//! be read is an error, never a free price.

use serde_json::Value;

use super::unreadable;
use crate::error::ApiError;
use crate::model::{
    AcceleratorTypeName, Money, PricedItem, Region, SeriesName, SkuPrice, UsageType,
};

/// What a row of [`ROWS`] prices.
#[derive(Debug, Clone, Copy)]
enum Row {
    /// The cores of the series.
    Cores(&'static str),
    /// The memory of the series.
    Memory(&'static str),
    /// The accelerator type of that name.
    Accelerator(&'static str),
}

impl Row {
    /// The item the row stands for.
    fn item(self) -> PricedItem {
        match self {
            Self::Cores(series) => PricedItem::Cores(SeriesName(series.to_owned())),
            Self::Memory(series) => PricedItem::Memory(SeriesName(series.to_owned())),
            Self::Accelerator(name) => {
                PricedItem::Accelerator(AcceleratorTypeName(name.to_owned()))
            }
        }
    }
}

/// The table: the words a SKU's description gives before " running in <region group>", and
/// the item they price. The descriptions are those of the Cloud Billing Catalog API's SKUs
/// of the Compute Engine service (`services/6F81-5844-456A`), as the Compute Engine pricing
/// pages list them. A description is matched whole, so "Nvidia Tesla A100 GPU" never reads
/// the "Nvidia Tesla A100 80GB GPU" SKU. The series of the cores and memory are those
/// `price.rs` prices; to price another, add its two rows here and its row there.
const ROWS: [(&str, Row); 16] = [
    ("N1 Predefined Instance Core", Row::Cores("n1")),
    ("N1 Predefined Instance Ram", Row::Memory("n1")),
    ("A2 Instance Core", Row::Cores("a2")),
    ("A2 Instance Ram", Row::Memory("a2")),
    ("G4 Instance Core", Row::Cores("g4")),
    ("G4 Instance Ram", Row::Memory("g4")),
    ("Nvidia Tesla T4 GPU", Row::Accelerator("nvidia-tesla-t4")),
    ("Nvidia Tesla P4 GPU", Row::Accelerator("nvidia-tesla-p4")),
    (
        "Nvidia Tesla V100 GPU",
        Row::Accelerator("nvidia-tesla-v100"),
    ),
    (
        "Nvidia Tesla A100 GPU",
        Row::Accelerator("nvidia-tesla-a100"),
    ),
    (
        "Nvidia Tesla A100 80GB GPU",
        Row::Accelerator("nvidia-a100-80gb"),
    ),
    ("Nvidia L4 GPU", Row::Accelerator("nvidia-l4")),
    ("Nvidia H100 80GB GPU", Row::Accelerator("nvidia-h100-80gb")),
    (
        "Nvidia H100 Mega 80GB GPU",
        Row::Accelerator("nvidia-h100-mega-80gb"),
    ),
    (
        "Nvidia RTX PRO 6000 GPU",
        Row::Accelerator("nvidia-rtx-pro-6000"),
    ),
    ("Nvidia B200 GPU", Row::Accelerator("nvidia-b200")),
];

/// The words that begin the description of a Spot SKU, before the words of [`ROWS`]
/// ("Spot Preemptible N1 Predefined Instance Core running in ...").
const LEAD_INS: [&str; 2] = ["Spot Preemptible ", "Preemptible "];

/// The words that end what a description prices and begin where.
const RUNNING_IN: &str = " running in ";

/// The item the SKU `description` prices, when a row of [`ROWS`] reads it.
fn item(description: &str) -> Option<PricedItem> {
    let words = LEAD_INS
        .iter()
        .find_map(|lead| description.strip_prefix(*lead))
        .unwrap_or(description);
    let (what, _) = words.split_once(RUNNING_IN)?;
    ROWS.iter()
        .find(|(name, _)| *name == what)
        .map(|(_, row)| row.item())
}

/// The usage type a SKU's `category.usageType` names, when the catalog reads it.
fn usage(sku: &Value) -> Option<UsageType> {
    match sku.pointer("/category/usageType").and_then(Value::as_str) {
        Some("OnDemand") => Some(UsageType::OnDemand),
        Some("Preemptible") => Some(UsageType::Preemptible),
        _ => None,
    }
}

/// Whether the SKU is offered in `region`.
fn offered_in(sku: &Value, region: &Region) -> bool {
    sku.get("serviceRegions")
        .and_then(Value::as_array)
        .is_some_and(|regions| {
            regions
                .iter()
                .any(|name| name.as_str() == Some(region.as_str()))
        })
}

/// The SKU a Cloud Billing `sku` answer entry describes, when it is offered in `region`,
/// is consumed on demand or as Spot, and a row of the table reads its description.
///
/// # Errors
///
/// [`ApiError::Unreadable`] when the SKU is one the catalog keeps and its price cannot be
/// read: no rate that starts at zero, or a unit price without a currency or with units or
/// nanos that are not whole numbers.
pub(super) fn read(sku: &Value, region: &Region) -> Result<Option<SkuPrice>, ApiError> {
    let kept = offered_in(sku, region).then(|| usage(sku)).flatten().zip(
        sku.get("description")
            .and_then(Value::as_str)
            .and_then(item),
    );
    match kept {
        None => Ok(None),
        Some((usage, item)) => Ok(Some(SkuPrice {
            item,
            usage,
            unit_price: unit_price(sku)?,
        })),
    }
}

/// The unit price of the first pricing's tier that starts at zero usage. A tier that
/// states no `startUsageAmount` starts at zero (proto3 JSON leaves out a zero).
fn unit_price(sku: &Value) -> Result<Money, ApiError> {
    let rates = sku
        .pointer("/pricingInfo/0/pricingExpression/tieredRates")
        .and_then(Value::as_array)
        .ok_or_else(|| unreadable("a SKU has no tiered rates"))?;
    let rate = rates
        .iter()
        .find(|rate| {
            rate.get("startUsageAmount")
                .is_none_or(|start| start.as_f64() == Some(0.0))
        })
        .ok_or_else(|| unreadable("a SKU has no rate that starts at zero"))?;
    let price = rate
        .get("unitPrice")
        .ok_or_else(|| unreadable("a rate has no unit price"))?;
    money(price)
}

/// The `Money` a `unitPrice` holds. `units` is the int64 as a string, or a number; absent
/// it is zero, as is an absent `nanos` (proto3 JSON leaves out a zero).
fn money(price: &Value) -> Result<Money, ApiError> {
    let currency = price
        .get("currencyCode")
        .and_then(Value::as_str)
        .ok_or_else(|| unreadable("a unit price has no currency"))?;
    let units = match price.get("units") {
        None => "0".to_owned(),
        Some(Value::String(text)) => text.clone(),
        Some(other) => other
            .as_i64()
            .map(|units| units.to_string())
            .ok_or_else(|| unreadable("units of a unit price is not a whole number"))?,
    };
    let nanos = match price.get("nanos") {
        None => 0,
        Some(value) => value
            .as_i64()
            .and_then(|nanos| i32::try_from(nanos).ok())
            .ok_or_else(|| unreadable("nanos of a unit price is not a whole number"))?,
    };
    Ok(Money {
        currency: currency.to_owned(),
        units,
        nanos,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    /// Asserts that every row of the table reads the description that names it, plain and
    /// after each lead-in of a Spot SKU, as the item the row stands for. It catches a row
    /// that mistypes its words, or a lead-in that is not stripped.
    #[test]
    fn every_row_reads_its_description() {
        for (name, row) in ROWS {
            for lead in ["", "Spot Preemptible ", "Preemptible "] {
                let description = format!("{lead}{name} running in Americas");
                assert_eq!(item(&description), Some(row.item()), "{description}");
            }
        }
    }

    /// Asserts that no two rows share their words or their item. It catches a row copied
    /// from another and left unchanged, which would shadow it.
    #[test]
    fn the_rows_have_distinct_words_and_items() {
        let names: BTreeSet<&str> = ROWS.iter().map(|(name, _)| *name).collect();
        assert_eq!(names.len(), ROWS.len());
        let items: BTreeSet<PricedItem> = ROWS.iter().map(|(_, row)| row.item()).collect();
        assert_eq!(items.len(), ROWS.len());
    }

    /// Asserts that a description no row reads has no item: words that do not end in the
    /// region group, words that match no row, and a row's words with others added. It
    /// catches a prefix match that reads a neighbouring SKU as this one.
    #[test]
    fn a_description_no_row_reads_has_no_item() {
        assert_eq!(item("N1 Predefined Instance Core"), None);
        assert_eq!(item("Storage PD Capacity running in Americas"), None);
        assert_eq!(item("Nvidia Tesla A100 GPU 2x running in Americas"), None);
        assert_eq!(item(""), None);
    }
}
