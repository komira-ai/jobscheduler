//! `--cpu-slack` and `server.cpu_slack`: how far above a host's cores the placement
//! engine may charge it, as a multiplier of its cores.
//!
//! The value is js-sched's own setting (`LoadConfig::s_cpu_permille`), written for the
//! operator as a multiplier: `1.1` is 100 thousandths above the cores. Its default and its
//! ceiling are js-sched's ([`LoadConfig::default`], [`MAX_CPU_SLACK_PERMILLE`]); neither is
//! written again here, so the two cannot drift. Nothing in this crate sets a value above
//! the default for the operator.

use std::fmt;

use js_sched::{LoadConfig, MAX_CPU_SLACK_PERMILLE};

/// The name of the key, as errors spell it.
const KEY: &str = "cpu_slack";

/// The most digits after the point: thousandths.
const DECIMALS: usize = 3;

/// The CPU slack of the placement engine: the thousandths above a host's cores it may be
/// charged, from 0 (1.0x) to [`MAX_CPU_SLACK_PERMILLE`] (2.0x). The default is
/// js-sched's, 1.1x.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuSlack(u32);

/// A `cpu_slack` that is not a multiplier from 1.0 to 2.0 with at most three decimals.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "{KEY} must be a multiplier of the cores from 1.0 to {max}, with at most three decimals, such as 1.1; got {given:?}",
    max = multiplier(MAX_CPU_SLACK_PERMILLE)
)]
pub struct CpuSlackError {
    /// The value as given.
    pub given: String,
}

impl Default for CpuSlack {
    fn default() -> Self {
        Self(LoadConfig::default().s_cpu_permille)
    }
}

impl CpuSlack {
    /// The slack `text` writes as a multiplier: digits, and optionally a point and up to
    /// three more digits, from `1.0` to `2.0`. A sign, a space, an exponent or an empty
    /// text is refused.
    ///
    /// # Errors
    /// [`CpuSlackError`] for anything else.
    pub fn parse(text: &str) -> Result<Self, CpuSlackError> {
        let refuse = || CpuSlackError {
            given: text.to_owned(),
        };
        let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
        let digits = |part: &str| part.bytes().all(|b| b.is_ascii_digit());
        let shaped = [
            !whole.is_empty(),
            digits(whole),
            digits(fraction),
            fraction.len() <= DECIMALS,
            !text.ends_with('.'),
        ];
        if !shaped.into_iter().all(|ok| ok) {
            return Err(refuse());
        }
        let padded = format!("{fraction:0<DECIMALS$}");
        let whole: u64 = whole.parse().map_err(|_| refuse())?;
        // Three ASCII digits, checked above: the parse cannot fail.
        let thousandths: u64 = padded.parse().unwrap_or(0);
        let total = whole.saturating_mul(1000).saturating_add(thousandths);
        let above = total.checked_sub(1000).ok_or_else(refuse)?;
        match u32::try_from(above) {
            Ok(above) if above <= MAX_CPU_SLACK_PERMILLE => Ok(Self(above)),
            _ => Err(refuse()),
        }
    }

    /// The thousandths above the cores: js-sched's `s_cpu_permille`.
    pub fn permille(self) -> u32 {
        self.0
    }

    /// js-sched's load settings with this slack and every other setting at its default.
    pub(crate) fn load_config(self) -> LoadConfig {
        LoadConfig {
            s_cpu_permille: self.0,
            ..LoadConfig::default()
        }
    }
}

impl fmt::Display for CpuSlack {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&multiplier(self.0))
    }
}

/// `above` thousandths over the cores as a multiplier, with at least one decimal and no
/// trailing zero after it: 100 is `1.1`, 1000 is `2.0`, 5 is `1.005`.
fn multiplier(above: u32) -> String {
    let total = 1000 + u64::from(above);
    let fraction = format!("{:03}", total % 1000);
    let fraction = fraction.trim_end_matches('0');
    let fraction = if fraction.is_empty() { "0" } else { fraction };
    format!("{}.{fraction}", total / 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: the default restated in this crate and drifting from js-sched's, and a
    /// slack read as a multiplier off by the one (1.1 taken as 1100 thousandths above).
    #[test]
    fn the_default_is_js_scheds_and_values_are_thousandths_above_the_cores() {
        let default = LoadConfig::default().s_cpu_permille;
        assert_eq!(CpuSlack::default().permille(), default);
        assert_eq!(CpuSlack::default().load_config(), LoadConfig::default());
        let cases = [
            ("1", 0),
            ("1.0", 0),
            ("1.1", 100),
            ("1.10", 100),
            ("1.005", 5),
            ("1.999", 999),
            ("2", 1000),
            ("2.000", 1000),
        ];
        for (text, permille) in cases {
            assert_eq!(CpuSlack::parse(text).map(CpuSlack::permille), Ok(permille), "{text}");
        }
        let slack = CpuSlack::parse("1.25").unwrap();
        assert_eq!(slack.load_config().s_cpu_permille, 250);
        assert_eq!(slack.load_config().s_mem_permille, 0);
    }

    /// Catches: a value below 1.0, above js-sched's ceiling, or not a plain decimal
    /// accepted, or clamped instead of refused. Each refusal names the key and the value
    /// exactly.
    #[test]
    fn values_outside_one_to_two_or_not_decimals_are_refused() {
        let bad = [
            "", "0.9", "0.999", "2.001", "2.5", "3", "abc", "1.", ".5", "1.0001", "-1.1",
            "+1.1", " 1.1", "1.1 ", "1e0", "1,1", "1..1", "1.a", "NaN", "inf",
            "99999999999999999999",
        ];
        for text in bad {
            let err = CpuSlack::parse(text).unwrap_err();
            assert_eq!(
                err.to_string(),
                format!(
                    "cpu_slack must be a multiplier of the cores from 1.0 to 2.0, with at most three decimals, such as 1.1; got {text:?}"
                ),
            );
        }
    }

    /// Catches: a multiplier printed with its trailing zeros, without a decimal, or off by
    /// the one.
    #[test]
    fn a_slack_prints_as_its_multiplier() {
        let shown = |text: &str| CpuSlack::parse(text).unwrap().to_string();
        assert_eq!(shown("1"), "1.0");
        assert_eq!(shown("1.1"), "1.1");
        assert_eq!(shown("1.005"), "1.005");
        assert_eq!(shown("1.250"), "1.25");
        assert_eq!(shown("2"), "2.0");
        assert_eq!(CpuSlack::default().to_string(), "1.1");
    }
}
