//! `--placement-curves` and `server.placement_curves`: whether the placement engine learns
//! a CPU and a memory profile for each job class from the series its runs recorded, and
//! whether it books by them. `off` learns nothing; `shadow` learns and logs one line per run
//! it checks, and places as `off` does; `on` learns and logs as `shadow` does and books a
//! class that passes its checks by its CPU curve, on the hosts `server.curve_hosts` names (or
//! every host), for at most `server.curve_max_wait_seconds` of a job's wait. The mode is read
//! by [`js_sched_placer::sizes::Curves::parse`]; this is where its refusal is worded, for
//! the flag and the key alike.

use js_sched_placer::curves::CurveSettings;
use js_sched_placer::sizes::Curves;

use crate::config::Config;

/// The curve mode of the placement engine: its `mode` (`--placement-curves`, or
/// `server.placement_curves`) and the `settings` that act only under `on`
/// (`server.curve_hosts` and `server.curve_max_wait_seconds`, which have no flags).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CurveSetting {
    /// `off` unless given.
    pub(crate) mode: Curves,
    /// The hosts that book by curve and the wait bound, read in every mode.
    pub(crate) settings: CurveSettings,
}

impl Config {
    /// The hosts that book by curve and the wait bound (`server.curve_hosts` and
    /// `server.curve_max_wait_seconds`): every host and 30 s unless the file names others.
    /// They act only when [`Config::placement_curves`] is `on`.
    #[must_use]
    pub fn curve_settings(&self) -> CurveSettings {
        self.agents.curves.settings.clone()
    }
}

/// `text` as the mode it names, or the refusal naming the three values that are accepted.
pub(crate) fn parse(text: &str) -> Result<Curves, String> {
    Curves::parse(text)
        .ok_or_else(|| format!("placement_curves must be off, shadow or on; got {text:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts that `off`, `shadow` and `on` are read as the mode they name, and that an
    /// upper-case spelling, a padded value and an empty one are refused with the whole
    /// message, which names the three accepted values and quotes the value given. Catches a
    /// parse that ignores case or trims, a refusal that does not name `on`, and an `on` that
    /// is refused.
    #[test]
    fn the_three_values_are_read_and_any_other_is_refused_in_full() {
        assert_eq!(parse("off"), Ok(Curves::Off));
        assert_eq!(parse("shadow"), Ok(Curves::Shadow));
        assert_eq!(parse("on"), Ok(Curves::On));
        for bad in ["On", " on", "Shadow", " off", ""] {
            assert_eq!(
                parse(bad),
                Err(format!(
                    "placement_curves must be off, shadow or on; got {bad:?}"
                )),
                "{bad:?}"
            );
        }
    }
}
