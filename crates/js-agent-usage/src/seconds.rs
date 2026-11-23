//! The whole host's CPU second by second: busy cores and CPU pressure, as rates between two
//! readings of the host's counters (`/proc/stat` and `/proc/pressure/cpu`, which the agent
//! reads).
//!
//! [`Seconds`] takes [`SecondSample`]s as values with the times they were taken at, and
//! reads no file and no clock, like the rest of the crate. Each sample after the first
//! gives one value for the interval since the one before: the host's busy CPU in
//! thousandths of a core (`usage_usec * 1000 / interval`), and the share of the interval
//! in which some task of the host waited for a CPU, in thousandths (`pressure_usec * 1000
//! / interval`, at most 1000). The values are kept, the newest `cap` of each, until
//! [`Seconds::take`] hands them over.
//!
//! - An interval is whatever the two samples are apart: a step that came late gives one
//!   value for the longer interval, not several.
//! - A counter that went down gives 0 for that interval: a
//!   rate is never invented from a negative difference.
//! - A sample whose time is not after the one before gives no value (it still becomes the
//!   baseline).
//! - Pressure is optional. A host without pressure files, or a sample whose pressure could
//!   not be read, gives no pressure value for that interval while the busy value stays, so
//!   the two lists may differ in length.

/// One second in microseconds: the least gap [`Seconds::due`] asks for between samples.
pub const SECOND_USEC: u64 = 1_000_000;

/// One reading of the host's CPU counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SecondSample {
    /// When the reading was taken, in microseconds on the monotonic clock.
    pub mono_usec: u64,
    /// The busy time of every CPU of the host, in microseconds: a counter that only grows.
    pub cpu_usage_usec: u64,
    /// The `some` total of the host's `/proc/pressure/cpu`, in microseconds, when it could be
    /// read.
    pub pressure_some_usec: Option<u64>,
}

/// The values [`Seconds`] kept, oldest first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SecondRates {
    /// Busy CPU of each interval, in thousandths of a core.
    pub cpu_millis: Vec<u32>,
    /// The share of each interval in which some task waited for a CPU, in thousandths, at
    /// most 1000.
    pub pressure_some_permille: Vec<u16>,
}

/// Turns samples into rates and keeps the newest of them. See the [module](self).
#[derive(Debug, Clone)]
pub struct Seconds {
    cap: usize,
    last: Option<SecondSample>,
    kept: SecondRates,
}

impl Seconds {
    /// A series that keeps at most `cap` values of each kind.
    pub fn new(cap: usize) -> Self {
        Self {
            cap,
            last: None,
            kept: SecondRates::default(),
        }
    }

    /// Whether a sample taken at `now_usec` is at least [`SECOND_USEC`] after the last one
    /// (always before the first).
    pub fn due(&self, now_usec: u64) -> bool {
        self.last
            .is_none_or(|last| now_usec.saturating_sub(last.mono_usec) >= SECOND_USEC)
    }

    /// Takes one sample: it becomes the baseline, and the interval since the previous one
    /// gives a value of each kind it can.
    pub fn observe(&mut self, sample: SecondSample) {
        let Some(before) = self.last.replace(sample) else {
            return;
        };
        let span = sample.mono_usec.saturating_sub(before.mono_usec);
        if span == 0 {
            return;
        }
        let rate =
            |now: u64, then: u64| u128::from(now.saturating_sub(then)) * 1000 / u128::from(span);
        let busy = rate(sample.cpu_usage_usec, before.cpu_usage_usec);
        push(
            &mut self.kept.cpu_millis,
            self.cap,
            u32::try_from(busy).unwrap_or(u32::MAX),
        );
        if let (Some(now), Some(then)) = (sample.pressure_some_usec, before.pressure_some_usec) {
            let share = rate(now, then).min(1000);
            push(
                &mut self.kept.pressure_some_permille,
                self.cap,
                u16::try_from(share).unwrap_or(1000),
            );
        }
    }

    /// The values kept so far, oldest first; the series starts empty again (its last
    /// sample stays the baseline of the next interval).
    pub fn take(&mut self) -> SecondRates {
        std::mem::take(&mut self.kept)
    }
}

/// Appends `value` and drops the oldest ones past `cap`.
fn push<T>(values: &mut Vec<T>, cap: usize, value: T) {
    values.push(value);
    let excess = values.len().saturating_sub(cap);
    values.drain(..excess);
}
