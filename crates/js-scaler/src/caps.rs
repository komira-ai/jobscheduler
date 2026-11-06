//! The caps (section 6.2, "Caps"), the nearest miss of the offers, and the billed time.
//! The offer a launch takes is pass 2's (`crate::pick`), not chosen here.

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
const DIMENSIONS: [Dimension; 13] = [
    Dimension::Os,
    Dimension::Arch,
    Dimension::IsaLevel,
    Dimension::Exec,
    Dimension::CpuMillis,
    Dimension::MemoryBytes,
    Dimension::DiskBytes,
    Dimension::AcceleratorVendor,
    Dimension::AcceleratorModel,
    Dimension::AcceleratorCount,
    Dimension::AcceleratorMemory,
    Dimension::AcceleratorInterconnect,
    Dimension::AcceleratorFormFactor,
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

/// The machines the pool holds against its caps: its records and the launches this step
/// staged that count, less those named in `freed`. Pass 2 counts the provider's machines
/// by the same rule.
pub(crate) fn holding<'r>(
    settings: &Settings,
    records: &'r [Record],
    staged: &'r [Record],
    freed: &BTreeSet<String>,
) -> Vec<&'r Record> {
    records
        .iter()
        .chain(staged)
        .filter(|record| {
            [
                record.pool == settings.pool,
                record.counts(),
                !freed.contains(record.name.as_str()),
            ]
            .into_iter()
            .all(|ok| ok)
        })
        .collect()
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
    let held = holding(settings, records, staged, freed);
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
