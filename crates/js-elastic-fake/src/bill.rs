//! What the fake cloud charges, kept apart from anything the scaler computes.

const SECOND: u64 = 1_000;
const MINUTE: u64 = 60 * SECOND;

/// How a provider rounds the time a machine ran into the time it bills.
///
/// The fake cloud owns this arithmetic. The scaler has its own (whole minutes, then a
/// whole cent); a test that compares the two can catch an accounting bug, which a test
/// of the scaler against itself cannot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Billing {
    /// Lambda: one-minute increments, each started minute billed whole, and at least one.
    Lambda,
    /// AWS: per second, with a 60-second minimum.
    Aws,
    /// Google Compute Engine: per second with no minimum. UNVERIFIED: the real granularity
    /// and minimum were not read when this was written, so the fake says so and defaults to
    /// per-second. Check it against Google's pricing terms before relying on a number.
    Gce,
    /// Google Cloud TPU: per second with no minimum. UNVERIFIED, for the same reason as
    /// [`Billing::Gce`].
    Tpu,
}

impl Billing {
    /// How many milliseconds a machine that ran `run_ms` is billed for.
    pub fn billed_ms(self, run_ms: u64) -> u64 {
        let up = |unit: u64| run_ms.div_ceil(unit).saturating_mul(unit);
        match self {
            Self::Lambda => up(MINUTE).max(MINUTE),
            Self::Aws => up(SECOND).max(MINUTE),
            Self::Gce | Self::Tpu => up(SECOND),
        }
    }
}

/// Money, in cent-milliseconds: a price in cents per hour times a time in milliseconds,
/// so that no division rounds anything away. An hour at one cent an hour is
/// [`Bill::HOUR_MS`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Bill(pub u128);

impl Bill {
    /// Milliseconds in an hour: the factor between a bill and cents.
    pub const HOUR_MS: u128 = 3_600_000;

    /// `cents` cents, as a bill.
    pub fn of_cents(cents: u64) -> Self {
        Self(u128::from(cents) * Self::HOUR_MS)
    }

    /// The bill in whole cents, rounded up.
    pub fn cents_up(self) -> u64 {
        u64::try_from(self.0.div_ceil(Self::HOUR_MS)).unwrap_or(u64::MAX)
    }
}
