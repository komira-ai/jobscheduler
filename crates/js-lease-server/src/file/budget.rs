//! The server-wide spend totals (`max_spend_per_day_total`, `max_spend_per_month_total`,
//! `max_spend_since_go_total`) in `[server]` and in `[budget.<kind>]`, and the pool's
//! `live_until`.

use std::collections::BTreeMap;

use js_scaler::{Budgets, Millis, Totals};
use toml::Value;
use toml::value::{Datetime, Offset};

use super::{Launch, LoadedHosts, Section};
use crate::error::StartupError;
use crate::kinds::Kinds;

/// The keys of a table of totals, in the order the reference lists them.
pub(super) const TOTAL_KEYS: &[&str] = &[
    "max_spend_per_day_total",
    "max_spend_per_month_total",
    "max_spend_since_go_total",
];

/// The totals the table gives, each a spend in cents, above zero. A month total below the
/// day total is refused: the day could never be spent.
pub(super) fn read_totals(s: &mut Section) -> Result<Totals, StartupError> {
    let totals = Totals {
        per_day: s.spend(TOTAL_KEYS[0])?,
        per_month: s.spend(TOTAL_KEYS[1])?,
        since_go: s.spend(TOTAL_KEYS[2])?,
    };
    if let (Some(day), Some(month)) = (totals.per_day, totals.per_month)
        && month < day
    {
        let reason = "must not be below max_spend_per_day_total";
        return Err(s.fail(TOTAL_KEYS[1], reason));
    }
    Ok(totals)
}

/// The budgets of the file: the `[server]` totals and one table of totals for each provider
/// kind in `[budget]`. A kind the server does not build is refused, so a typo does not
/// leave a cloud without its total.
pub(super) fn read_budgets(
    table: Option<toml::Table>,
    server: Totals,
    kinds: &Kinds,
) -> Result<Budgets, StartupError> {
    let mut budgets = Budgets {
        server,
        kinds: BTreeMap::new(),
    };
    let known = kinds.names();
    for (kind, value) in table.unwrap_or_default() {
        let place = format!("budget.{kind}");
        if !known.contains(&kind.as_str()) {
            let reason = format!("not a provider kind; known: {}", known.join(", "));
            return Err(StartupError::at(place, reason));
        }
        let Value::Table(inner) = value else {
            return Err(StartupError::at(place, "must be a table"));
        };
        let mut s = Section::open(inner, format!("budget.{kind}."));
        s.only(TOTAL_KEYS)?;
        let totals = read_totals(&mut s)?;
        if totals == Totals::default() {
            return Err(StartupError::at(place, "sets no total"));
        }
        if let (Some(day), Some(month)) = (server.per_day, totals.per_month)
            && month < day
        {
            let reason = "must not be below server.max_spend_per_day_total";
            return Err(s.fail(TOTAL_KEYS[1], reason));
        }
        budgets.kinds.insert(kind, totals);
    }
    Ok(budgets)
}

/// The warnings about the go of the elastic pool of `hosts`, if it has one:
/// - `live_until` has passed: the pool starts in plan, and the operator who wrote
///   `launch = "live"` should hear why it does not launch;
/// - `live_until` is set on a pool that is in plan: it has no effect, and the pool will not
///   go live when it is reached;
/// - a total is set and the pool is live without a `max_age` or a `max_lease_wall`: the
///   totals then bound nothing about a machine that runs on, since a busy host is never
///   retired and the overrun bound ([`js_scaler::overrun`]) needs both.
pub(super) fn notes(hosts: &LoadedHosts, budgets: &Budgets, now: Millis) -> Vec<String> {
    let LoadedHosts::Registering { registering, .. } = hosts else {
        return Vec::new();
    };
    let Some(plan) = registering.plan.as_ref() else {
        return Vec::new();
    };
    let name = &plan.name;
    let mut said = Vec::new();
    if let Some(until) = plan.live_until {
        if now >= until {
            said.push(format!(
                "pool {name:?}: live_until has passed, so the pool is in plan mode and launches nothing; set a later live_until to go live"
            ));
        } else if plan.launch == Launch::Plan {
            said.push(format!(
                "pool {name:?}: live_until has no effect on a pool whose launch is plan"
            ));
        }
    }
    let totals = [budgets.server]
        .into_iter()
        .chain(budgets.kinds.values().copied())
        .any(|each| each != Totals::default());
    let unbounded = [plan.max_age.is_none(), plan.max_lease_wall.is_none()];
    if totals && plan.launch == Launch::Live && unbounded.contains(&true) {
        said.push(format!(
            "pool {name:?}: spend totals are set but the pool has no max_age or no server.max_lease_wall, so a host that stays busy is never retired and a total bounds only the launches, not what the machines already launched cost"
        ));
    }
    said
}

impl Section {
    /// An instant written as a TOML date-time with an offset, such as
    /// `2027-01-01T12:00:00Z`. A date-time with no date, no time or no offset is refused:
    /// without an offset the instant depends on the server's time zone.
    pub(super) fn timestamp(&mut self, key: &str) -> Result<Option<Millis>, StartupError> {
        let must = "must be a date and time with an offset, such as 2027-01-01T12:00:00Z";
        let datetime = match self.map.remove(key) {
            None => return Ok(None),
            Some(Value::Datetime(datetime)) => datetime,
            Some(_) => return Err(self.fail(key, must)),
        };
        match epoch_millis(&datetime) {
            Some(Ok(millis)) => Ok(Some(Millis::new(millis))),
            Some(Err(())) => Err(self.fail(key, "must not be before the epoch")),
            None => Err(self.fail(key, must)),
        }
    }
}

/// Milliseconds since the epoch of `at`, if it names a date, a time and an offset; `Err` when
/// the instant is before the epoch.
fn epoch_millis(at: &Datetime) -> Option<Result<u64, ()>> {
    let (date, time, offset) = (at.date?, at.time?, at.offset?);
    let minutes = match offset {
        Offset::Z => 0,
        Offset::Custom { minutes } => i64::from(minutes),
    };
    let days = days_from_civil(
        i64::from(date.year),
        i64::from(date.month),
        i64::from(date.day),
    );
    let seconds = days * 86_400
        + i64::from(time.hour) * 3_600
        + i64::from(time.minute) * 60
        + i64::from(time.second.unwrap_or_default())
        - minutes * 60;
    let millis = seconds * 1_000 + i64::from(time.nanosecond.unwrap_or_default() / 1_000_000);
    Some(u64::try_from(millis).map_err(|_| ()))
}

/// The days from 1970-01-01 to the given day of the proleptic Gregorian calendar.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let of_era = year - era * 400;
    let shifted = (month + 9) % 12;
    let of_year = (153 * shifted + 2) / 5 + day - 1;
    let days_of_era = of_era * 365 + of_era / 4 - of_era / 100 + of_year;
    era * 146_097 + days_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(text: &str) -> Result<Option<Millis>, StartupError> {
        let table: toml::Table = text.parse().unwrap();
        Section::open(table, "pool \"gpu\": ".to_owned()).timestamp("live_until")
    }

    /// Asserts the date-time conversion against instants worked out by hand: a UTC time, the
    /// same instant written in two offsets, a leap day, and a fraction of a second.
    #[test]
    fn a_timestamp_is_read_as_milliseconds_since_the_epoch() {
        let expected = Some(Millis::new(1_798_761_600_000));
        for text in [
            "live_until = 2027-01-01T00:00:00Z",
            "live_until = 2027-01-01T01:00:00+01:00",
            "live_until = 2026-12-31T19:00:00-05:00",
        ] {
            assert_eq!(read(text).unwrap(), expected, "{text}");
        }
        // 2028-03-01 is 365 + 365 + 31 + 29 days after 2026-01-01 (2028 is a leap year).
        let leap = read("live_until = 2028-03-01T00:00:00Z").unwrap();
        let days = 365 + 365 + 31 + 29;
        assert_eq!(
            leap,
            Some(Millis::new((1_767_225_600 + days * 86_400) * 1_000))
        );
        let part = read("live_until = 2027-01-01T00:00:00.250Z").unwrap();
        assert_eq!(part, Some(Millis::new(1_798_761_600_250)));
    }

    /// Asserts that a value that is not a full instant is refused, naming the key and not
    /// repeating the value: a bare date, a time with no date, a date and time with no offset, a string and an integer.
    #[test]
    fn a_timestamp_without_a_date_a_time_or_an_offset_is_refused() {
        for text in [
            "live_until = 2027-01-01",
            "live_until = 2027-01-01T00:00:00",
            "live_until = 07:32:00",
            "live_until = \"2027-01-01T00:00:00Z\"",
            "live_until = 3",
        ] {
            let said = read(text).unwrap_err().to_string();
            assert!(said.contains("live_until"), "{said}");
            assert!(said.contains("with an offset"), "{said}");
        }
        assert_eq!(read("").unwrap(), None);
    }

    /// Asserts that an instant before the epoch is refused with a reason that says so, not
    /// the reason for a missing offset: the date-time is complete, it is only too early.
    #[test]
    fn a_timestamp_before_the_epoch_is_refused_as_such() {
        use toml::value::{Date, Time};
        let early = Datetime {
            date: Some(Date {
                year: 1,
                month: 1,
                day: 1,
            }),
            time: Some(Time {
                hour: 0,
                minute: 0,
                second: Some(0),
                nanosecond: Some(0),
            }),
            offset: Some(Offset::Z),
        };
        let table = toml::Table::from_iter([("live_until".to_owned(), Value::Datetime(early))]);
        let said = Section::open(table, "pool \"gpu\": ".to_owned())
            .timestamp("live_until")
            .unwrap_err()
            .to_string();
        assert_eq!(
            said,
            "pool \"gpu\": live_until: must not be before the epoch"
        );
    }
}
