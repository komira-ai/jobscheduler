//! The caps (section 6.2, "Caps"), the choice among offers, and the billed time.

use std::collections::BTreeSet;
use std::time::Duration;

use js_elastic::{Demand, Dimension, Offer};

use crate::record::{Phase, Record};
use crate::settings::{Blocked, Settings};
use crate::time::Millis;

const MINUTE: Duration = Duration::from_secs(60);

/// The window of the day cap.
pub(crate) const DAY: Duration = Duration::from_secs(24 * 60 * 60);

/// The order in which [`Offer::fit`] checks, which is how near a miss is.
const DIMENSIONS: [Dimension; 8] = [
    Dimension::Os,
    Dimension::Arch,
    Dimension::Exec,
    Dimension::CpuMillis,
    Dimension::MemoryBytes,
    Dimension::DiskBytes,
    Dimension::Gpus,
    Dimension::GpuClass,
];

/// How far down [`Offer::fit`]'s checks `dimension` is.
fn depth(dimension: Dimension) -> usize {
    DIMENSIONS
        .iter()
        .position(|each| *each == dimension)
        .unwrap_or_default()
}

/// The dimension the offers miss by least, or `None` when one of them fits or there are
/// none.
pub(crate) fn nearest_miss<'a>(
    offers: impl Iterator<Item = &'a Offer>,
    demand: &Demand,
) -> Option<Dimension> {
    let misses: Option<Vec<Dimension>> = offers.map(|offer| offer.fit(demand).err()).collect();
    misses.and_then(|all| all.into_iter().max_by_key(|each| depth(*each)))
}

/// Whether `offer` fits `demand`.
pub(crate) fn fits(offer: &Offer, demand: &Demand) -> bool {
    let relaxed = Demand {
        os: None,
        arch: None,
        ..demand.clone()
    };
    offer.fit(&relaxed).is_ok()
}

/// The offer a launch for `demand` takes: available, priced, fitting, and not refused
/// this step; the cheapest, then the one with fewest gpus, then by id, then by zone.
pub(crate) fn choose<'a>(
    settings: &Settings,
    offers: &'a [Offer],
    demand: &Demand,
    refused: &[(String, String, Option<String>)],
) -> Option<&'a Offer> {
    offers
        .iter()
        .filter(|offer| {
            let lately = refused.iter().any(|(pool, id, zone)| {
                (pool.as_str(), id.as_str(), zone.as_deref())
                    == (
                        settings.pool.as_str(),
                        offer.id.as_str(),
                        offer.zone.as_deref(),
                    )
            });
            [
                offer.available,
                offer.price_cents_per_hour.is_some(),
                fits(offer, demand),
                !lately,
            ]
            .into_iter()
            .all(|ok| ok)
        })
        .min_by_key(|offer| {
            (
                offer.price_cents_per_hour,
                offer.gpus,
                offer.id.clone(),
                offer.zone.clone(),
            )
        })
}

/// What `price` per hour costs for `time`, rounded up to whole minutes and then to a
/// whole cent.
fn cents(price: u64, time: Duration) -> u64 {
    let minutes = time.as_millis().div_ceil(MINUTE.as_millis());
    let cost = (u128::from(price) * minutes).div_ceil(60);
    u64::try_from(cost).unwrap_or(u64::MAX)
}

/// The least a machine of `price` costs: a boot that fails runs to `boot_timeout`.
pub(crate) fn least_cost(settings: &Settings, price: u64) -> u64 {
    cents(price, settings.boot_timeout.max(MINUTE))
}

/// The least time a record is billed for, or `None` when it never cost anything: a
/// launch still waiting, and one the provider refused.
fn billed_at_least(record: &Record, settings: &Settings) -> Option<Duration> {
    match record.phase {
        Phase::Planned | Phase::Refused => None,
        Phase::Orphan => Some(MINUTE),
        _ if record.registered => Some(MINUTE),
        _ => Some(settings.boot_timeout.max(MINUTE)),
    }
}

/// What a record has cost: its stored price over the time from launch to gone (or to
/// `now`), at least a minute, and for a boot that never said hello at least
/// `boot_timeout`.
fn spent(record: &Record, settings: &Settings, now: Millis) -> u64 {
    billed_at_least(record, settings).map_or(0, |least| {
        let end = record.gone_at.unwrap_or(now);
        cents(
            record.price_cents_per_hour,
            end.since(record.launched_at).max(least),
        )
    })
}

/// Whether one more machine of `price` may be launched into the pool.
///
/// `records` are the pool's machines, `staged` the launches this step has already made,
/// and `freed` the names of machines that count as free because they are idle and would
/// be released.
///
/// # Errors
///
/// The first cap it breaks: instances, then spend per hour, then spend per day.
pub(crate) fn check(
    settings: &Settings,
    now: Millis,
    records: &[Record],
    staged: &[Record],
    freed: &BTreeSet<String>,
    price: u64,
) -> Result<(), Blocked> {
    let ours: Vec<&Record> = records
        .iter()
        .chain(staged)
        .filter(|record| record.pool == settings.pool)
        .collect();
    let held: Vec<&Record> = ours
        .iter()
        .copied()
        .filter(|record| {
            [record.counts(), !freed.contains(record.name.as_str())]
                .into_iter()
                .all(|ok| ok)
        })
        .collect();
    let hourly = held.iter().fold(0u64, |sum, record| {
        sum.saturating_add(record.price_cents_per_hour)
    });
    let day = ours
        .iter()
        .copied()
        .filter(|record| now.since(record.launched_at) < DAY)
        .fold(0u64, |sum, record| {
            sum.saturating_add(spent(record, settings, now))
        });
    [
        (held.len() < settings.max_instances, Blocked::MaxInstances),
        (
            hourly.saturating_add(price) <= settings.max_spend_per_hour,
            Blocked::MaxSpend,
        ),
        (
            day.saturating_add(least_cost(settings, price)) <= settings.max_spend_per_day,
            Blocked::MaxSpendDay,
        ),
    ]
    .into_iter()
    .find(|(within, _)| !within)
    .map_or(Ok(()), |(_, blocked)| Err(blocked))
}
