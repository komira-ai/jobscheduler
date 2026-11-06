//! Prices in US cents per hour, read from the decimal text of the cloud's answers with
//! integer arithmetic: no float, so no rounding drift. A fraction of a cent rounds up, so
//! an offer never states a price below what the cloud charges (and a price of a tenth of a
//! cent is 1, never a free 0). Text that is not a decimal is an error, never zero.

use std::collections::BTreeMap;

use crate::error::ApiError;
use crate::model::{BlockOffering, InstanceType, TypePrice, Zone, ZonePrice};

/// The only currency the catalog reads: offers are priced in US cents.
const USD: &str = "USD";

/// `text`, a decimal number of US dollars (`32.7726`, `2`, `0.0010`), in US cents, a
/// fraction of a cent rounded up.
///
/// # Errors
///
/// [`ApiError::Unreadable`] when `text` is not digits with at most one `.` and a digit
/// before it, or its cents do not fit 64 bits.
pub(crate) fn cents(text: &str) -> Result<u64, ApiError> {
    let unreadable = || ApiError::Unreadable(format!("{text:?} is not a price in US dollars"));
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    let digits = |part: &str| part.bytes().all(|b| b.is_ascii_digit());
    let readable = [!whole.is_empty(), digits(whole), digits(fraction)]
        .into_iter()
        .all(|ok| ok);
    if !readable {
        return Err(unreadable());
    }
    let dollars: u64 = whole.parse().map_err(|_| unreadable())?;
    let mut fraction = fraction.bytes().map(|b| u64::from(b - b'0'));
    let tenths = fraction.next().unwrap_or(0);
    let hundredths = fraction.next().unwrap_or(0);
    let rest = u64::from(fraction.any(|digit| digit > 0));
    dollars
        .checked_mul(100)
        .and_then(|cents| cents.checked_add(tenths * 10 + hundredths + rest))
        .ok_or_else(unreadable)
}

/// The on-demand price of each type, in cents per hour. A type the answer prices twice
/// (it should not) takes the higher price, so the answer's order never matters.
///
/// # Errors
///
/// [`ApiError::Unreadable`] naming the first price that is not a decimal.
pub(crate) fn on_demand(prices: &[TypePrice]) -> Result<BTreeMap<InstanceType, u64>, ApiError> {
    let mut table = BTreeMap::new();
    for price in prices {
        let cents = cents(&price.usd_per_hour)?;
        let entry = table.entry(price.instance_type.clone()).or_insert(cents);
        *entry = (*entry).max(cents);
    }
    Ok(table)
}

/// The spot price of each type in each zone, in cents per hour; a pair priced twice takes
/// the higher price.
///
/// # Errors
///
/// [`ApiError::Unreadable`] naming the first price that is not a decimal.
pub(crate) fn spot(prices: &[ZonePrice]) -> Result<BTreeMap<(InstanceType, Zone), u64>, ApiError> {
    let mut table = BTreeMap::new();
    for price in prices {
        let cents = cents(&price.usd_per_hour)?;
        let key = (price.instance_type.clone(), price.zone.clone());
        let entry = table.entry(key).or_insert(cents);
        *entry = (*entry).max(cents);
    }
    Ok(table)
}

/// The upfront price of a Capacity Block offering, in cents.
///
/// # Errors
///
/// [`ApiError::Unreadable`] when the offering is not priced in US dollars or its fee is
/// not a decimal.
pub(crate) fn upfront(offering: &BlockOffering) -> Result<u64, ApiError> {
    match offering.currency == USD {
        true => cents(&offering.upfront_fee),
        false => Err(ApiError::Unreadable(format!(
            "capacity block {} is priced in {:?}, not USD",
            offering.id, offering.currency
        ))),
    }
}

/// A block's upfront price as a price per instance per hour, rounded up: `upfront_cents`
/// over `instances` instances for `minutes` minutes. `None` for a block of no instance or
/// no time, or one whose numbers overflow.
pub(crate) fn hourly_share(upfront_cents: u64, instances: u32, minutes: u64) -> Option<u64> {
    let machine_minutes = u64::from(instances)
        .checked_mul(minutes)
        .filter(|total| *total > 0)?;
    upfront_cents
        .checked_mul(60)
        .map(|numerator| numerator.div_ceil(machine_minutes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unreadable(text: &str) -> bool {
        matches!(cents(text), Err(ApiError::Unreadable(_)))
    }

    /// Asserts the conversion of the decimal text to cents by rounding a fraction of a
    /// cent up: `1.5000` is 150, `32.7726` is 3278, `0.0010` is 1, `0.07` is 7, `2` and
    /// `2.` are 200, `0.0700000001` is 8, and the largest price that fits is kept exactly.
    /// Catches float drift, truncation, and a fraction read as zero.
    #[test]
    fn decimal_dollars_become_cents_rounded_up() {
        let cases = [
            ("1.5000", 150),
            ("32.7726", 3278),
            ("0.0010", 1),
            ("0.07", 7),
            ("0.1", 10),
            ("2", 200),
            ("2.", 200),
            ("0.0000", 0),
            ("0.0700000001", 8),
            ("32.7726000000", 3278),
            ("184467440737095516.15", u64::MAX),
        ];
        for (text, expected) in cases {
            assert_eq!(cents(text), Ok(expected), "{text}");
        }
    }

    /// Asserts that text that is not a decimal of dollars is unreadable and never a price:
    /// empty, a bare `.`, a sign, an exponent, letters, two points, spaces, and numbers
    /// whose cents do not fit 64 bits (in the dollars and in the rounding). The error
    /// quotes the text in full.
    #[test]
    fn text_that_is_not_a_decimal_is_unreadable() {
        for text in [
            "",
            ".",
            ".5",
            "-1",
            "+1",
            "1e3",
            "abc",
            "1.2.3",
            " 1",
            "1,5",
            "NaN",
            "99999999999999999999999",
            "184467440737095517",
            "184467440737095516.16",
        ] {
            assert!(unreadable(text), "{text:?}");
        }
        assert_eq!(
            cents("1e3"),
            Err(ApiError::Unreadable(
                "\"1e3\" is not a price in US dollars".to_owned()
            ))
        );
    }

    /// Asserts that a type or a zone priced twice takes the higher price, whichever comes
    /// first, and that an unreadable price fails the whole table.
    #[test]
    fn a_price_given_twice_takes_the_higher() {
        let kind = InstanceType("p5.48xlarge".to_owned());
        let zone = Zone("zone-a".to_owned());
        let type_price = |usd: &str| TypePrice {
            instance_type: kind.clone(),
            usd_per_hour: usd.to_owned(),
        };
        let zone_price = |usd: &str| ZonePrice {
            instance_type: kind.clone(),
            zone: zone.clone(),
            usd_per_hour: usd.to_owned(),
        };
        for order in [["1", "2"], ["2", "1"]] {
            let table = on_demand(&order.map(type_price)).unwrap();
            assert_eq!(table.get(&kind), Some(&200));
            let table = spot(&order.map(zone_price)).unwrap();
            assert_eq!(table.get(&(kind.clone(), zone.clone())), Some(&200));
        }
        assert!(on_demand(&[type_price("x")]).is_err());
        assert!(spot(&[zone_price("x")]).is_err());
    }

    /// Asserts that a block's price per instance hour is its upfront price over its
    /// instance hours, rounded up, and that a block of no instance, no time, or numbers
    /// that overflow has none; that a block priced in another currency is unreadable.
    #[test]
    fn a_block_price_is_spread_over_its_instance_hours() {
        assert_eq!(hourly_share(240_000, 2, 24 * 60), Some(5000));
        assert_eq!(hourly_share(100, 3, 60), Some(34));
        assert_eq!(hourly_share(100, 0, 60), None);
        assert_eq!(hourly_share(100, 1, 0), None);
        assert_eq!(hourly_share(u64::MAX, 1, 60), None);
        assert_eq!(hourly_share(1, u32::MAX, u64::MAX), None);
        let offering = |currency: &str| BlockOffering {
            id: "cb-1".to_owned(),
            instance_type: InstanceType("p5.48xlarge".to_owned()),
            zone: Zone("zone-a".to_owned()),
            instance_count: 1,
            start_unix_seconds: 0,
            duration_minutes: 60,
            upfront_fee: "31.464".to_owned(),
            currency: currency.to_owned(),
        };
        assert_eq!(upfront(&offering("USD")), Ok(3147));
        assert_eq!(
            upfront(&offering("EUR")),
            Err(ApiError::Unreadable(
                "capacity block cb-1 is priced in \"EUR\", not USD".to_owned()
            ))
        );
    }
}
