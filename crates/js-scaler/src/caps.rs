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

/// The window of the monthly totals: 30 days, rolling. A calendar month would let a total
/// be spent twice in a few days across the turn of the month, and would make the figure
/// depend on a calendar and a time zone the scaler does not have; a rolling window holds
/// at every instant.
pub(crate) const MONTH: Duration = Duration::from_secs(30 * 24 * 60 * 60);

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

/// Whether `offer` could serve `demand` but for its price: available, fitting, and not
/// refused this step.
fn usable(
    settings: &Settings,
    offer: &Offer,
    demand: &Demand,
    refused: &[(String, String, Option<String>)],
) -> bool {
    let lately = refused.iter().any(|(pool, id, zone)| {
        (pool.as_str(), id.as_str(), zone.as_deref())
            == (
                settings.pool.as_str(),
                offer.id.as_str(),
                offer.zone.as_deref(),
            )
    });
    [offer.available, offer.fit(demand).is_ok(), !lately]
        .into_iter()
        .all(|ok| ok)
}

/// Whether `offer` has a price above zero. A price of zero is no price: it would pass both
/// spend caps for any number of machines.
fn priced(offer: &Offer) -> bool {
    offer.price_cents_per_hour.is_some_and(|price| price > 0)
}

/// Whether some offer would serve `demand` but for having no price, or a price of zero.
/// [`choose`] never takes one, so the pool launches nothing for the request, and the
/// operator is told why.
pub(crate) fn unpriced(
    settings: &Settings,
    offers: &[Offer],
    demand: &Demand,
    refused: &[(String, String, Option<String>)],
) -> bool {
    offers.iter().any(|offer| {
        [usable(settings, offer, demand, refused), !priced(offer)]
            .into_iter()
            .all(|ok| ok)
    })
}

/// The offer a launch for `demand` takes: available, priced above zero, fitting, and not
/// refused this step; the cheapest, then the one with fewest gpus, then by id, then by
/// zone.
pub(crate) fn choose<'a>(
    settings: &Settings,
    offers: &'a [Offer],
    demand: &Demand,
    refused: &[(String, String, Option<String>)],
) -> Option<&'a Offer> {
    offers
        .iter()
        .filter(|offer| {
            [usable(settings, offer, demand, refused), priced(offer)]
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

/// The last instant a record is billed to: when it was confirmed gone, or now while it
/// counts and runs; never before the least it is billed for. A launch still waiting for
/// its time, and a record that no longer counts and has no end, are billed the least and
/// nothing more. A refused acquire is billed nothing.
///
/// The least is a minute, and for a boot that never said hello (a failed boot, an acquire
/// never seen, a launch waiting) `boot_timeout`. `boot_timeout` is that of the record's
/// pool.
pub(crate) fn billed_until(record: &Record, boot_timeout: Duration, now: Millis) -> Millis {
    let least = match (record.phase, record.registered) {
        (Phase::Refused, _) => Duration::ZERO,
        (Phase::Orphan, _) | (_, true) => MINUTE,
        _ => boot_timeout.max(MINUTE),
    };
    let end = match (record.phase, record.counts()) {
        (Phase::Planned, _) | (_, false) => record.gone_at.unwrap_or(record.launched_at),
        _ => record.gone_at.unwrap_or(now),
    };
    end.max(record.launched_at.plus(least))
}

/// What a record has cost from `from` on: its stored price over the part of its billed time
/// (see [`billed_until`]) that lies after `from` and after its launch, rounded up to whole
/// minutes and then a whole cent. A machine that still runs keeps accruing, and one that
/// ended before `from` costs nothing.
pub(crate) fn spent_since(
    record: &Record,
    boot_timeout: Duration,
    now: Millis,
    from: Millis,
) -> u64 {
    let until = billed_until(record, boot_timeout, now);
    let from = record.launched_at.max(from);
    cents(record.price_cents_per_hour, until.since(from))
}

/// What a record has cost inside the last 24 hours. A machine that still runs keeps
/// accruing, so a long-lived or stuck one goes on eating the day cap, and one that ended
/// more than a day ago costs nothing.
fn spent(record: &Record, settings: &Settings, now: Millis) -> u64 {
    spent_since(record, settings.boot_timeout, now, now.back(DAY))
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
    let day = ours.iter().fold(0u64, |sum, record| {
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
