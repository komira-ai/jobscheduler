//! The server-wide spend budgets: totals over all the pools of the server, or over all the
//! pools of one provider kind, that a launch must hold beside the pool's own caps.
//!
//! The pool's caps ([`Settings`](crate::Settings)) count one pool's machines over the last
//! 24 hours. A day cap D allows 30 times D in a month left alone, and the exposure of a
//! server is the sum of its pools' caps. A total counts the machines of every pool in its
//! scope, over a longer or a different window, so one figure bounds what the operator can
//! lose however many pools there are.
//!
//! Money is in cents, as the pool's caps are. A total only keeps a launch from being made
//! ([`Blocked::MaxSpendTotal`](crate::Blocked::MaxSpendTotal)): it never drains or releases
//! a machine, so a machine admitted under it runs to its own end and its cost can take the
//! window past the figure; see [`overrun`].

use std::collections::BTreeMap;
use std::time::Duration;

use crate::caps::{DAY, MONTH, least_cost, spent_since};
use crate::input::Input;
use crate::record::{Go, Record};
use crate::settings::{Blocked, Settings};
use crate::time::Millis;

/// The totals of one scope. A key left `None` is not checked.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Totals {
    /// The most the machines of the scope may cost over the last 24 hours, and one more.
    pub per_day: Option<u64>,
    /// The most they may cost over the last 30 days (a rolling window, not a calendar
    /// month), and one more.
    pub per_month: Option<u64>,
    /// The most they may cost since their pool's go ([`Go`]), and one more: a budget that
    /// a pool brings when it is switched to live and takes away when it is switched back
    /// or its `live_until` comes.
    pub since_go: Option<u64>,
}

/// The totals the server holds: one for the whole server and one for each provider kind
/// that has any. A launch must hold the server's and its pool's kind's.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Budgets {
    /// Over every pool, and every record, including one of a pool this server no longer
    /// has.
    pub server: Totals,
    /// Over the pools of a [`Settings::kind`], by kind.
    pub kinds: BTreeMap<String, Totals>,
}

/// The most a window can pass its total by, in cents, when the admitted machines run to
/// their ends: `hourly` (the pool's `max_spend_per_hour`, which bounds the price of what is
/// held at any one time) for `lifetime` (how long a machine runs after it is admitted;
/// [`Settings::lifetime`] bounds it when the pool has a `max_age`).
///
/// A launch is admitted when the window's cost plus the least the new machine costs
/// (`boot_timeout` of it) is within the total, so after the last admission the machines
/// held are all the window can still gain from. No more than `hourly` per hour of them are
/// held at once, and none longer than `lifetime`. The rounding of each record up to a
/// whole cent adds up to a cent for each machine counted in the window; `machines` is how
/// many that can be.
#[must_use]
pub fn overrun(hourly: u64, lifetime: Duration, machines: u64) -> u64 {
    let hours = lifetime.as_millis().div_ceil(3_600_000);
    let cost = u128::from(hourly).saturating_mul(hours);
    u64::try_from(cost)
        .unwrap_or(u64::MAX)
        .saturating_add(machines)
}

/// Whether `spent` plus `least` is within `total`, or there is no total. `spent` is not
/// computed for a window with no total.
fn within(total: Option<u64>, least: u64, spent: impl FnOnce() -> u64) -> bool {
    total.is_none_or(|total| spent().saturating_add(least) <= total)
}

/// Whether one more machine of `price` may be launched into the pool `settings`, once
/// `records` and the `staged` launches of the step are counted, against the server's
/// totals and those of the pool's kind. Both must hold.
///
/// # Errors
///
/// [`Blocked::MaxSpendTotal`] when a total of either scope would be passed, in the day, the
/// month or since the go.
pub(crate) fn check(
    input: &Input,
    go: &BTreeMap<String, Go>,
    settings: &Settings,
    now: Millis,
    (records, staged): (&[Record], &[Record]),
    price: u64,
) -> Result<(), Blocked> {
    let least = least_cost(settings, price);
    let kind = input.budgets.kinds.get(&settings.kind).copied();
    let scopes = [
        (input.budgets.server, None),
        (kind.unwrap_or_default(), Some(&settings.kind)),
    ];
    for (totals, only) in scopes {
        let member = |pool: &str| only.is_none_or(|kind| input.kind_of(pool) == Some(kind));
        let all = records
            .iter()
            .chain(staged)
            .filter(|record| member(record.pool.as_str()));
        let window = |from: Millis| {
            all.clone().fold(0u64, |sum, record| {
                let timeout = input.boot_timeout(&record.pool);
                sum.saturating_add(spent_since(record, timeout, now, from))
            })
        };
        let since_go = || {
            let settled = go
                .iter()
                .filter(|(pool, _)| member(pool.as_str()))
                .fold(0u64, |sum, (_, each)| sum.saturating_add(each.settled));
            all.clone().fold(settled, |sum, record| {
                let timeout = input.boot_timeout(&record.pool);
                let spent = go
                    .get(&record.pool)
                    .map_or(0, |each| spent_since(record, timeout, now, each.at));
                sum.saturating_add(spent)
            })
        };
        let checks = [
            within(totals.per_day, least, || window(now.back(DAY))),
            within(totals.per_month, least, || window(now.back(MONTH))),
            within(totals.since_go, least, since_go),
        ];
        if checks.contains(&false) {
            return Err(Blocked::MaxSpendTotal);
        }
    }
    Ok(())
}
