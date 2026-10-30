//! The dedupe: each alert is said once while it holds, said again when it has held for
//! `repeat_after`, and never lost to a refusal.

use std::collections::BTreeMap;
use std::time::Duration;

use crate::sink::Alert;

/// The first wait after a refusal; each further refusal doubles it.
pub const RETRY_BASE: Duration = Duration::from_secs(5);

/// The longest wait after refusals.
pub const RETRY_CAP: Duration = Duration::from_secs(600);

/// Where one alert stands with the sink.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Standing {
    /// Not delivered: to be sent at the first pass at or after this time.
    Unsent(Duration),
    /// Handed to the delivery and not yet answered.
    InFlight,
    /// Delivered at this time.
    Delivered(Duration),
}

#[derive(Debug, Clone)]
struct Entry {
    text: String,
    /// Whether the alert held at the last pass.
    holding: bool,
    /// Refusals since the last delivery.
    refusals: u32,
    standing: Standing,
}

/// The state of the alerts the sink has been given, with time as an input.
///
/// - An alert is sent at the first pass it holds, and again at the first pass at or after
///   `repeat_after` since it was last delivered while it still holds, so one missed
///   message does not leave a leak announced only once.
/// - One that clears and returns after its message was delivered is said again.
/// - A refused alert is sent again after a wait that doubles with each refusal (from
///   [`RETRY_BASE`] to [`RETRY_CAP`]), and it stays pending until it has been delivered
///   once, even if it clears in the meantime.
#[derive(Debug, Clone)]
pub struct Said {
    repeat_after: Duration,
    entries: BTreeMap<String, Entry>,
}

impl Said {
    /// A state that repeats a holding alert every `repeat_after`.
    pub fn new(repeat_after: Duration) -> Self {
        Self {
            repeat_after,
            entries: BTreeMap::new(),
        }
    }

    /// Takes the alerts that hold at time `now` (key to text) and returns the ones to send
    /// now. Each returned alert is in flight until [`Said::delivered`] or
    /// [`Said::refused`] says how it went.
    pub fn pass(&mut self, now: Duration, found: &BTreeMap<String, String>) -> Vec<Alert> {
        for entry in self.entries.values_mut() {
            entry.holding = false;
        }
        for (key, text) in found {
            let entry = self.entries.entry(key.clone()).or_insert_with(|| Entry {
                text: String::new(),
                holding: true,
                refusals: 0,
                standing: Standing::Unsent(now),
            });
            entry.holding = true;
            entry.text.clone_from(text);
        }
        // A cleared alert is forgotten once delivered; until then it stays pending.
        self.entries
            .retain(|_, entry| entry.holding || !matches!(entry.standing, Standing::Delivered(_)));
        let repeat_after = self.repeat_after;
        let mut out = Vec::new();
        for (key, entry) in &mut self.entries {
            let due = match entry.standing {
                Standing::Unsent(at) => at <= now,
                Standing::InFlight => false,
                // Only a holding alert is left in the state `Delivered` by the `retain` above.
                Standing::Delivered(at) => now.saturating_sub(at) >= repeat_after,
            };
            if due {
                entry.standing = Standing::InFlight;
                out.push(Alert {
                    key: key.clone(),
                    text: entry.text.clone(),
                });
            }
        }
        out
    }

    /// The sink took the alert `key`, at time `now`.
    pub fn delivered(&mut self, key: &str, now: Duration) {
        self.entries.entry(key.to_owned()).and_modify(|entry| {
            entry.refusals = 0;
            entry.standing = Standing::Delivered(now);
        });
    }

    /// The sink refused the alert `key`, at time `now`: it is sent again after a wait.
    pub fn refused(&mut self, key: &str, now: Duration) {
        self.entries.entry(key.to_owned()).and_modify(|entry| {
            entry.refusals = entry.refusals.saturating_add(1);
            let doublings = entry.refusals.saturating_sub(1).min(16);
            let wait = RETRY_BASE.saturating_mul(1 << doublings).min(RETRY_CAP);
            entry.standing = Standing::Unsent(now + wait);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: Duration = Duration::from_secs(3600);

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn holding(keys: &[&str]) -> BTreeMap<String, String> {
        keys.iter()
            .map(|key| ((*key).to_owned(), format!("about {key}")))
            .collect()
    }

    fn keys(sent: &[Alert]) -> Vec<&str> {
        sent.iter().map(|alert| alert.key.as_str()).collect()
    }

    /// Asserts the dedupe rule: an alert that holds is returned once however many passes
    /// follow, a second alert when it appears and not before, and one that clears and
    /// returns after delivery is returned again. It catches a flood every tick and a sink
    /// that goes quiet for good after the first alert.
    #[test]
    fn each_key_is_said_once_and_again_after_it_clears_and_returns() {
        let mut said = Said::new(HOUR);
        let first = said.pass(secs(0), &holding(&["a"]));
        assert_eq!(keys(&first), ["a"]);
        assert_eq!(first[0].text, "about a");
        said.delivered("a", secs(0));
        assert!(said.pass(secs(1), &holding(&["a"])).is_empty());
        let second = said.pass(secs(2), &holding(&["a", "b"]));
        assert_eq!(keys(&second), ["b"]);
        said.delivered("b", secs(2));
        assert!(said.pass(secs(3), &holding(&["b"])).is_empty());
        let again = said.pass(secs(4), &holding(&["a", "b"]));
        assert_eq!(keys(&again), ["a"]);
    }

    /// Asserts that a holding alert is said again at exactly `repeat_after` after its
    /// delivery and not a second before, and that each repeat starts the next period. It
    /// catches a leak announced only once: a missed message would leave a host billing
    /// unseen for as long as the alert holds.
    #[test]
    fn a_holding_alert_repeats_at_exactly_repeat_after() {
        let mut said = Said::new(HOUR);
        said.pass(secs(10), &holding(&["a"]));
        said.delivered("a", secs(10));
        assert!(
            said.pass(secs(10) + HOUR - secs(1), &holding(&["a"]))
                .is_empty()
        );
        let due = said.pass(secs(10) + HOUR, &holding(&["a"]));
        assert_eq!(keys(&due), ["a"]);
        // In flight: not sent twice while the answer is awaited.
        assert!(
            said.pass(secs(10) + HOUR + secs(1), &holding(&["a"]))
                .is_empty()
        );
        said.delivered("a", secs(10) + HOUR + secs(1));
        assert!(
            said.pass(secs(10) + HOUR + HOUR, &holding(&["a"]))
                .is_empty()
        );
        let next = said.pass(secs(11) + HOUR + HOUR, &holding(&["a"]));
        assert_eq!(keys(&next), ["a"]);
    }

    /// Asserts that a refusal is retried after a wait that doubles (5 s, 10 s, 20 s) and
    /// is capped, and that a delivery resets it. It catches an unbounded retry on every
    /// pass against a receiver that is down, and a wait that never ends.
    #[test]
    fn a_refused_alert_waits_longer_each_time_up_to_a_cap() {
        let mut said = Said::new(HOUR);
        let mut now = secs(0);
        assert_eq!(keys(&said.pass(now, &holding(&["a"]))), ["a"]);
        for wait in [5, 10, 20] {
            said.refused("a", now);
            assert!(said.pass(now + secs(wait - 1), &holding(&["a"])).is_empty());
            now += secs(wait);
            assert_eq!(keys(&said.pass(now, &holding(&["a"]))), ["a"], "{wait}");
        }
        for _ in 0..20 {
            said.refused("a", now);
        }
        assert!(
            said.pass(now + RETRY_CAP - secs(1), &holding(&["a"]))
                .is_empty()
        );
        now += RETRY_CAP;
        assert_eq!(keys(&said.pass(now, &holding(&["a"]))), ["a"]);
        said.delivered("a", now);
        said.pass(now, &holding(&[]));
        assert_eq!(keys(&said.pass(now, &holding(&["a"]))), ["a"]);
        said.refused("a", now);
        assert!(said.pass(now + secs(4), &holding(&["a"])).is_empty());
        assert_eq!(keys(&said.pass(now + secs(5), &holding(&["a"]))), ["a"]);
    }

    /// Asserts that an alert refused and then cleared before the next pass is still
    /// delivered, once, and forgotten after; and that a refused-and-cleared alert that
    /// returns before its retry is one pending alert, not two. It catches an alert lost
    /// because the condition ended while the receiver was down.
    #[test]
    fn a_refused_alert_is_delivered_even_if_it_has_cleared() {
        let mut said = Said::new(HOUR);
        said.pass(secs(0), &holding(&["a"]));
        said.refused("a", secs(0));
        assert!(said.pass(secs(1), &holding(&[])).is_empty());
        let retry = said.pass(secs(5), &holding(&[]));
        assert_eq!(keys(&retry), ["a"]);
        assert_eq!(retry[0].text, "about a");
        said.delivered("a", secs(5));
        assert!(said.pass(secs(6), &holding(&[])).is_empty());
        // Forgotten: it holds again, so it is said again at once.
        assert_eq!(keys(&said.pass(secs(7), &holding(&["a"]))), ["a"]);

        let mut said = Said::new(HOUR);
        said.pass(secs(0), &holding(&["b"]));
        said.refused("b", secs(0));
        said.pass(secs(1), &holding(&[]));
        assert!(said.pass(secs(2), &holding(&["b"])).is_empty());
        assert_eq!(keys(&said.pass(secs(5), &holding(&["b"]))), ["b"]);
    }

    /// Asserts that an answer for an alert nobody tracks changes nothing. It catches an
    /// answer that makes an entry out of nothing.
    #[test]
    fn an_answer_for_an_unknown_alert_is_ignored() {
        let mut said = Said::new(HOUR);
        said.delivered("x", secs(0));
        said.refused("x", secs(0));
        assert!(said.pass(secs(1), &holding(&[])).is_empty());
    }
}
