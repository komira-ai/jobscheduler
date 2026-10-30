//! The watcher: one tick compares the cloud with the lease server's own account of itself
//! and says what is wrong through an alert sink.
//!
//! It holds a [`CloudView`] (reads), a [`StatusSource`] (reads) and an [`AlertSink`], and
//! nothing that spends: it has no handle to make or end anything, so it only counts and
//! reports. Time is an input of [`Watcher::tick`], so a test moves it by hand.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use js_alert::{AlertSink, Notifier};
use tracing::info;

use crate::status::{Machine, StatusSource};
use crate::view::CloudView;

/// The most alerts waiting for the sink; beyond it the oldest is dropped and counted.
const ALERT_QUEUE: usize = 64;

/// How the watcher judges what it reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    /// How long the server's tick time may stay the same (or the page stay unreadable)
    /// before the heartbeat alarm.
    pub stale_after: Duration,
    /// How many ticks in a row an instance must be unexplained before it is reported: a
    /// launch shows in the provider's list a moment before the server's records show it.
    pub confirm_ticks: u32,
    /// The time between two sweeps for leftovers.
    pub sweep_every: Duration,
    /// How often an alert that keeps holding is sent again.
    pub repeat_after: Duration,
}

/// What one tick found, for the log and for tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    /// The scheduler's instances the cloud lists.
    pub instances: usize,
    /// Of those, the ones the server's status does not show, for long enough to report.
    pub unexplained: usize,
    /// The leftovers the last sweep found.
    pub strays: usize,
    /// The alerts that hold now.
    pub alerts: usize,
}

/// The watcher.
pub struct Watcher {
    cloud: Arc<dyn CloudView>,
    status: Arc<dyn StatusSource>,
    notifier: Notifier,
    settings: Settings,
    /// The last time the status page was read, or the watcher's start before any read.
    readable_at: Option<Duration>,
    /// For each pool, the last tick time the server showed and when the watcher first saw it.
    beats: BTreeMap<String, (Option<u64>, Duration)>,
    /// The machines the server showed on the last read that worked.
    expected: Option<Vec<Machine>>,
    /// For each unexplained instance, in how many ticks in a row.
    unexplained: BTreeMap<String, u32>,
    /// The leftovers of the last sweep that worked, as alerts by key.
    strays: BTreeMap<String, String>,
    /// Why the last sweep failed, when it did.
    sweep_failed: Option<String>,
    swept_at: Option<Duration>,
}

impl Watcher {
    /// A watcher that reads `cloud` and `status` and alerts through `sink`, delivered on a
    /// thread of its own (see [`Notifier`]).
    pub fn new(
        (cloud, status): (Arc<dyn CloudView>, Arc<dyn StatusSource>),
        sink: Arc<dyn AlertSink>,
        settings: Settings,
    ) -> Self {
        Self {
            cloud,
            status,
            notifier: Notifier::new(sink, settings.repeat_after, ALERT_QUEUE),
            settings,
            readable_at: None,
            beats: BTreeMap::new(),
            expected: None,
            unexplained: BTreeMap::new(),
            strays: BTreeMap::new(),
            sweep_failed: None,
            swept_at: None,
        }
    }

    /// One look at the world at time `now` (any origin that never goes back).
    pub fn tick(&mut self, now: Duration) -> Report {
        let mut found = BTreeMap::new();
        self.heartbeat(now, &mut found);
        let instances = self.compare(&mut found);
        self.sweep(now, &mut found);
        let confirm = self.settings.confirm_ticks;
        let report = Report {
            instances,
            unexplained: self.unexplained.values().filter(|n| **n >= confirm).count(),
            strays: self.strays.len(),
            alerts: found.len(),
        };
        self.notifier.pass(now, &found);
        report
    }

    /// Waits until every alert handed to the sink has been answered.
    pub fn flush(&self) {
        self.notifier.flush();
    }

    /// Runs ticks until `pause` says to stop: `clock` gives each tick's time and `pause`
    /// waits between ticks and returns whether to go on.
    pub fn drive(&mut self, mut clock: impl FnMut() -> Duration, mut pause: impl FnMut() -> bool) {
        loop {
            let report = self.tick(clock());
            info!(?report, "watch tick");
            if !pause() {
                break;
            }
        }
        self.notifier.flush();
    }

    /// Alerts when the status page has not been readable for `stale_after`, and for each
    /// pool whose tick time has not changed for `stale_after`: one wedged pool alarms
    /// whatever the others do.
    fn heartbeat(&mut self, now: Duration, found: &mut BTreeMap<String, String>) {
        let readable_at = *self.readable_at.get_or_insert(now);
        match self.status.read() {
            Ok(status) => {
                self.readable_at = Some(now);
                self.expected = Some(status.machines);
                self.beats.retain(|pool, _| status.beats.contains_key(pool));
                for (pool, beat) in status.beats {
                    let seen = self.beats.entry(pool.clone()).or_insert((beat, now));
                    if seen.0 != beat {
                        *seen = (beat, now);
                    }
                    let silent = now.saturating_sub(seen.1);
                    if silent >= self.settings.stale_after {
                        found.insert(
                            format!("heartbeat:{pool}"),
                            format!(
                                "the lease server has not ticked pool {pool} for {} seconds: its status answers but the tick time does not move; check the server, and if it is down check for machines still running",
                                silent.as_secs()
                            ),
                        );
                    }
                }
            }
            Err(why) => {
                let silent = now.saturating_sub(readable_at);
                if silent >= self.settings.stale_after {
                    found.insert(
                        "heartbeat".to_owned(),
                        format!(
                            "the lease server's status has not been readable for {} seconds ({why}); check the server, and if it is down check for machines still running",
                            silent.as_secs()
                        ),
                    );
                }
            }
        }
    }

    /// Lists the cloud and alerts on each instance the server's status does not show.
    /// Returns how many instances the cloud lists.
    fn compare(&mut self, found: &mut BTreeMap<String, String>) -> usize {
        let cloud = self.cloud.cloud().to_owned();
        let seen = match self.cloud.instances() {
            Ok(seen) => seen,
            Err(why) => {
                let text = format!("cannot list the {cloud} instances ({why}); nothing is checked");
                found.insert(format!("list:{cloud}"), text);
                return 0;
            }
        };
        let Some(expected) = &self.expected else {
            return seen.len();
        };
        // A machine is matched by its provider id when it has one. Only a machine with no
        // id yet is matched by name, and each such name explains ONE instance: a second
        // instance with that name (a launch whose answer was lost and was launched again)
        // is not explained.
        let ids: BTreeSet<&str> = expected
            .iter()
            .filter_map(|machine| machine.provider_id.as_deref())
            .collect();
        let mut by_name: BTreeSet<&str> = expected
            .iter()
            .filter(|machine| machine.provider_id.is_none())
            .map(|machine| machine.host.as_str())
            .collect();
        let confirm = self.settings.confirm_ticks;
        let mut counts = BTreeMap::new();
        for each in &seen {
            let explained = ids.contains(each.id.as_str())
                || each
                    .name
                    .as_deref()
                    .is_some_and(|name| by_name.remove(name));
            if explained {
                continue;
            }
            let ticks = self.unexplained.get(&each.id).copied().unwrap_or(0) + 1;
            counts.insert(each.id.clone(), ticks);
            if ticks >= confirm {
                let name = each.name.as_deref().unwrap_or("none");
                found.insert(
                    format!("unexplained:{cloud}:{}", each.id),
                    format!(
                        "the {cloud} instance {} (name {name}, region {}) is in the provider's list and the lease server's status does not show it; it is billing and nothing owns it: end it by hand or with `js-watch abort`",
                        each.id, each.region
                    ),
                );
            }
        }
        self.unexplained = counts;
        seen.len()
    }

    /// Lists the leftovers when a sweep is due, and alerts on what the last sweep found.
    fn sweep(&mut self, now: Duration, found: &mut BTreeMap<String, String>) {
        let due = self
            .swept_at
            .is_none_or(|last| now.saturating_sub(last) >= self.settings.sweep_every);
        if due {
            self.swept_at = Some(now);
            self.sweep_failed = None;
            match self.cloud.strays() {
                Ok(strays) => self.strays = strays.iter().map(alert_of).collect(),
                Err(why) => self.sweep_failed = Some(why.to_string()),
            }
        }
        found.extend(self.strays.clone());
        if let Some(why) = &self.sweep_failed {
            let cloud = self.cloud.cloud();
            found.insert(
                format!("sweep:{cloud}"),
                format!("cannot sweep the {cloud} account for leftovers ({why})"),
            );
        }
    }
}

/// The key and the words of the alert for one leftover.
fn alert_of(stray: &crate::view::Stray) -> (String, String) {
    let cost = stray.cents_per_hour.map_or_else(
        || "at a price the cloud did not say".to_owned(),
        |cents| format!("at {}.{:02} USD per hour", cents / 100, cents % 100),
    );
    let key = format!("stray:{}:{}:{}", stray.kind, stray.region, stray.id);
    let text = format!(
        "a leftover {} {} in {} carries this scheduler's marks and bills {cost}; the watcher counts it and does not touch it: remove it at the cloud yourself",
        stray.kind, stray.id, stray.region
    );
    (key, text)
}
