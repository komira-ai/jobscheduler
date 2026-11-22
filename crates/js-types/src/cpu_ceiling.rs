//! An optional CPU ceiling for a job, as a multiple of its booked CPU ([`CpuCeiling`]), as an
//! agent's operator sets it (`--cpu-ceiling`). A job's CPU booking is what placement packs a
//! host by and what the job is guaranteed; nothing holds a job below what it needs, and with
//! no ceiling (the default) a job also uses every core no other job wants. A ceiling holds a
//! job to the booking times the multiple; at a multiple of 1 a job is held to its booking.

use std::fmt;
use std::str::FromStr;

/// The largest multiple a ceiling may be, in thousandths: a ceiling above it holds nothing a
/// host has.
const MOST_PERMILLE: u32 = 1_000_000;

/// A CPU ceiling of a job as a multiple of its booked CPU, in thousandths: `1` (1000) holds a
/// job to its booking, `2.5` (2500) to two and a half times it. At least 1, at most 1000, with at
/// most three decimals ([`FromStr`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CpuCeiling(u32);

/// Why a text is not a [`CpuCeiling`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CpuCeilingError {
    /// The text is not a decimal number of at most three decimals, such as `1`, `1.5` or
    /// `2.25`.
    #[error(
        "{0:?} is not a multiple of the booking such as 1, 1.5 or 2.25 (at most three decimals)"
    )]
    NotAMultiple(String),
    /// The multiple is below 1, which would hold a job below what it booked and is guaranteed,
    /// or above 1000.
    #[error("a CPU ceiling is a multiple of the booking from 1 to 1000, not {0}")]
    OutOfRange(String),
}

impl CpuCeiling {
    /// The ceiling at `permille` thousandths of the booking, when that is from 1 to 1000 times.
    #[must_use]
    pub fn from_permille(permille: u32) -> Option<Self> {
        (1000..=MOST_PERMILLE)
            .contains(&permille)
            .then_some(Self(permille))
    }

    /// The multiple in thousandths of the booking.
    #[must_use]
    pub fn permille(self) -> u32 {
        self.0
    }

    /// The most CPU a lease that booked `cpu_millis` may use under this ceiling, in thousandths
    /// of a core, rounded up; `None` when nothing was booked (no ceiling is written).
    #[must_use]
    pub fn millis_of(self, cpu_millis: u64) -> Option<u64> {
        let most = u128::from(cpu_millis) * u128::from(self.0);
        (cpu_millis > 0).then(|| u64::try_from(most.div_ceil(1000)).unwrap_or(u64::MAX))
    }
}

impl FromStr for CpuCeiling {
    type Err = CpuCeilingError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let not = || CpuCeilingError::NotAMultiple(text.to_owned());
        let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
        let digits = |part: &str| part.bytes().all(|b| b.is_ascii_digit());
        let shape = !whole.is_empty() && digits(whole) && digits(fraction);
        if !shape || fraction.len() > 3 || (text.contains('.') && fraction.is_empty()) {
            return Err(not());
        }
        let whole: u32 = whole
            .parse()
            .map_err(|_| CpuCeilingError::OutOfRange(text.to_owned()))?;
        let thousandths: u32 = format!("{fraction:0<3}").parse().map_err(|_| not())?;
        whole
            .checked_mul(1000)
            .and_then(|whole| whole.checked_add(thousandths))
            .and_then(Self::from_permille)
            .ok_or_else(|| CpuCeilingError::OutOfRange(text.to_owned()))
    }
}

impl fmt::Display for CpuCeiling {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (whole, part) = (self.0 / 1000, self.0 % 1000);
        match part {
            0 => write!(f, "{whole}"),
            _ => write!(f, "{}", format!("{whole}.{part:03}").trim_end_matches('0')),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a multiple read wrongly, a ceiling below the booking taken (it would hold a job
    /// below what it is guaranteed), and text that is not a plain decimal taken.
    #[test]
    fn a_ceiling_is_a_decimal_multiple_from_one_to_a_thousand() {
        for (text, permille) in [
            ("1", 1000),
            ("1.5", 1500),
            ("2.25", 2250),
            ("1.125", 1125),
            ("01.50", 1500),
            ("1000", 1_000_000),
        ] {
            assert_eq!(
                text.parse::<CpuCeiling>().map(CpuCeiling::permille),
                Ok(permille),
                "{text}"
            );
        }
        for text in ["0.999", "0", "1000.001", "4294968"] {
            assert_eq!(
                text.parse::<CpuCeiling>(),
                Err(CpuCeilingError::OutOfRange(text.to_owned())),
                "{text}"
            );
        }
        for text in ["", "1.", ".5", "1.2345", "-1", "1e3", "one", "1,5", " 1"] {
            assert_eq!(
                text.parse::<CpuCeiling>(),
                Err(CpuCeilingError::NotAMultiple(text.to_owned())),
                "{text:?}"
            );
        }
        assert_eq!(CpuCeiling::from_permille(999), None);
    }

    /// Catches a ceiling shown in a form it does not parse back from.
    #[test]
    fn a_ceiling_shows_as_the_multiple_it_was_given() {
        for text in ["1", "1.5", "2.25", "1.125", "1000"] {
            let ceiling: CpuCeiling = text.parse().expect("a ceiling");
            assert_eq!(ceiling.to_string(), text);
        }
    }

    /// Catches a ceiling below the booking times the multiple (rounding down holds a job below
    /// it), and one for a job that booked nothing.
    #[test]
    fn a_ceiling_is_the_booking_times_the_multiple_rounded_up() {
        let ceiling = |text: &str| text.parse::<CpuCeiling>().expect("a ceiling");
        assert_eq!(ceiling("1").millis_of(500), Some(500));
        assert_eq!(ceiling("2").millis_of(250), Some(500));
        assert_eq!(ceiling("1.5").millis_of(1), Some(2));
        assert_eq!(ceiling("1000").millis_of(u64::MAX), Some(u64::MAX));
        assert_eq!(ceiling("2").millis_of(0), None);
    }
}
