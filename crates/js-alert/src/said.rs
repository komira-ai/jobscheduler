//! The dedupe: each alert is said once while it holds.

use std::collections::{BTreeMap, BTreeSet};

use tracing::warn;

use crate::sink::{Alert, AlertSink};

/// The keys of the alerts already delivered and still holding.
#[derive(Debug, Clone, Default)]
pub struct Said(BTreeSet<String>);

impl Said {
    /// Delivers, through `sink`, each alert of `found` (key to text) that was not already
    /// said, and forgets the said alerts that are no longer in `found`, so that one that
    /// comes back is said again. An alert the sink refuses is not marked said: the next
    /// call sends it again.
    pub fn deliver(&mut self, found: &BTreeMap<String, String>, sink: &dyn AlertSink) {
        self.0.retain(|key| found.contains_key(key));
        for (key, text) in found {
            if self.0.contains(key) {
                continue;
            }
            let alert = Alert {
                key: key.clone(),
                text: text.clone(),
            };
            match sink.send(&alert) {
                Ok(()) => {
                    self.0.insert(key.clone());
                }
                Err(why) => warn!(%why, %key, "cannot deliver an alert; it is sent again"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sink::FakeSink;

    fn holding(keys: &[&str]) -> BTreeMap<String, String> {
        keys.iter()
            .map(|key| ((*key).to_owned(), format!("about {key}")))
            .collect()
    }

    /// Asserts the dedupe rule: an alert that holds across many passes is delivered once,
    /// a second alert is delivered when it appears and not before, and an alert that
    /// clears and returns is delivered again. It catches a sink that is flooded every
    /// tick, and one that goes quiet for good after the first alert.
    #[test]
    fn each_key_is_said_once_and_again_after_it_clears_and_returns() {
        let sink = FakeSink::new();
        let mut said = Said::default();
        said.deliver(&holding(&["a"]), &sink);
        said.deliver(&holding(&["a"]), &sink);
        assert_eq!(sink.keys(), ["a"]);
        said.deliver(&holding(&["a", "b"]), &sink);
        assert_eq!(sink.keys(), ["a", "b"]);
        said.deliver(&holding(&["b"]), &sink);
        said.deliver(&holding(&["a", "b"]), &sink);
        assert_eq!(sink.keys(), ["a", "b", "a"]);
        assert_eq!(sink.sent()[2].text, "about a");
    }

    /// Asserts that an alert the sink refused is not counted as said, so it is sent at
    /// the next pass, once. It catches an alert lost for good because the webhook was
    /// down at the moment it first held.
    #[test]
    fn a_refused_alert_is_sent_again_until_it_goes_through() {
        let sink = FakeSink::new();
        let mut said = Said::default();
        sink.refuse_next(2);
        said.deliver(&holding(&["a"]), &sink);
        assert!(sink.sent().is_empty());
        said.deliver(&holding(&["a"]), &sink);
        assert!(sink.sent().is_empty());
        said.deliver(&holding(&["a"]), &sink);
        said.deliver(&holding(&["a"]), &sink);
        assert_eq!(sink.keys(), ["a"]);
    }
}
