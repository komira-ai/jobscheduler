//! The most CPU a run used over any five seconds of its life ([`PeakWindow`]), from the
//! `usage_usec` of its lease's `cpu.stat` read each time the run is polled, at most once a
//! second, on the agent's monotonic clock. What the job's usage reports as
//! [`JobUsage::cpu_peak`](js_agent_wire::JobUsage::cpu_peak): the CPU it needs at its busiest,
//! which the history books it (a compile that needs 20 cores for 30 seconds shows 20 000 m).

use std::collections::VecDeque;

use js_agent_wire::CpuPeak;

/// The span a peak is measured over, in microseconds: five seconds.
pub(super) const WINDOW_USEC: u64 = 5_000_000;
/// The least time between two samples kept, in microseconds: one second. A poll sooner than
/// that after the last sample kept adds nothing.
pub(super) const EVERY_USEC: u64 = 1_000_000;

/// The samples of one run and the highest rate seen over a span of at least [`WINDOW_USEC`].
/// Pure: the caller reads the clock and the counter.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct PeakWindow {
    /// The first sample: `(time, usage)`, both in microseconds.
    first: Option<(u64, u64)>,
    /// The samples kept, oldest first: the newest one at least a window before the last, and
    /// every one after it.
    kept: VecDeque<(u64, u64)>,
    /// The highest rate over a full window so far.
    best: Option<CpuPeak>,
}

impl PeakWindow {
    /// Takes the sample `usage` CPU microseconds at `at` microseconds, unless it comes less
    /// than [`EVERY_USEC`] after the last one kept.
    pub(super) fn observe(&mut self, at: u64, usage: u64) {
        if let Some(&(last, _)) = self.kept.back()
            && at.saturating_sub(last) < EVERY_USEC
        {
            return;
        }
        self.take(at, usage);
    }

    /// The peak of the run, with its last sample `usage` at `at`: the highest rate over any
    /// span of a window or more, or, for a run shorter than a window, its whole-run mean with
    /// the window its length. `None` for a run with no time between its first sample and
    /// this one.
    pub(super) fn finish(mut self, at: u64, usage: u64) -> Option<CpuPeak> {
        self.take(at, usage);
        if self.best.is_some() {
            return self.best;
        }
        let (start, used) = self.first.unwrap_or((at, usage));
        rate(at.saturating_sub(start), usage.saturating_sub(used))
    }

    fn take(&mut self, at: u64, usage: u64) {
        self.first.get_or_insert((at, usage));
        self.kept.push_back((at, usage));
        // Keep the newest sample a window or more before this one, and drop those before it.
        while self
            .kept
            .get(1)
            .is_some_and(|&(next, _)| at.saturating_sub(next) >= WINDOW_USEC)
        {
            self.kept.pop_front();
        }
        // Never empty: this sample was just kept.
        let (from, used) = self.kept.front().copied().unwrap_or((at, usage));
        let span = at.saturating_sub(from);
        if span < WINDOW_USEC {
            return;
        }
        let now = rate(span, usage.saturating_sub(used));
        if now.map(|now| now.millicores) > self.best.map(|best| best.millicores) {
            self.best = now;
        }
    }
}

/// `used` CPU microseconds over `span` microseconds, in millicores; `None` for no span.
fn rate(span: u64, used: u64) -> Option<CpuPeak> {
    (span > 0).then(|| CpuPeak {
        millicores: u64::try_from(u128::from(used) * 1000 / u128::from(span)).unwrap_or(u64::MAX),
        window_usec: span,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = 1_000_000;

    fn peak(millicores: u64, window_usec: u64) -> Option<CpuPeak> {
        Some(CpuPeak {
            millicores,
            window_usec,
        })
    }

    /// Feeds `samples` of `(seconds, cpu seconds used so far)` and ends at the last.
    fn run(samples: &[(u64, u64)]) -> Option<CpuPeak> {
        let mut window = PeakWindow::default();
        let (&(end, used), rest) = samples.split_last().expect("a sample");
        for &(at, usage) in rest {
            window.observe(at * S, usage * S);
        }
        window.finish(end * S, used * S)
    }

    /// Catches a peak taken as the run's mean: a run idle for a minute that then needs 20
    /// cores for 30 seconds reports 20 cores over five seconds, not its mean of 6.7.
    #[test]
    fn a_burst_of_twenty_cores_reads_as_twenty_cores_not_the_mean() {
        let mut samples: Vec<(u64, u64)> = (0..=60).map(|at| (at, 0)).collect();
        samples.extend((61..=90).map(|at| (at, (at - 60) * 20)));
        samples.extend((91..=95).map(|at| (at, 600)));
        assert_eq!(run(&samples), peak(20_000, 5 * S));
    }

    /// Catches a run shorter than the window reporting nothing, or a rate over a span it did
    /// not run: two seconds at 3 cores reads as its whole-run mean, 3 cores over 2 seconds.
    #[test]
    fn a_run_shorter_than_the_window_reports_its_mean_over_its_length() {
        assert_eq!(run(&[(0, 0), (1, 3), (2, 6)]), peak(3000, 2 * S));
    }

    /// Catches polls sooner than a second apart kept (the window would hold hundreds of
    /// samples) or counted: polls every tenth of a second keep the one-second samples, and the
    /// peak is the same as with one sample a second.
    #[test]
    fn polls_sooner_than_a_second_apart_add_nothing() {
        let mut window = PeakWindow::default();
        for tenth in 0..=100 {
            window.observe(tenth * S / 10, tenth * S / 10 * 4);
        }
        assert!(window.kept.len() <= 7, "{}", window.kept.len());
        assert_eq!(window.finish(10 * S, 40 * S), peak(4000, 5 * S));
    }

    /// Catches a window measured over the gap between rare polls as if it were five seconds:
    /// samples 8 seconds apart give the rate over those 8 seconds, with that window.
    #[test]
    fn a_gap_longer_than_the_window_is_measured_over_its_own_length() {
        assert_eq!(run(&[(0, 0), (8, 16), (16, 24)]), peak(2000, 8 * S));
    }

    /// Catches a run with no time between its first and last sample (or only the last)
    /// reporting a rate: there is none to report.
    #[test]
    fn a_run_with_no_time_between_its_samples_has_no_peak() {
        assert_eq!(PeakWindow::default().finish(5 * S, 1), None);
        let mut window = PeakWindow::default();
        window.observe(3 * S, 7);
        assert_eq!(window.finish(3 * S, 9), None);
        assert_eq!(rate(0, 1), None);
    }

    /// Catches a counter that fell (a group made again) read as a huge rise, and a rate that
    /// overflows: a fall counts as no use, and the largest counter saturates.
    #[test]
    fn a_falling_counter_is_no_use_and_a_huge_one_saturates() {
        assert_eq!(run(&[(0, 9), (5, 1)]), peak(0, 5 * S));
        let mut window = PeakWindow::default();
        window.observe(0, 0);
        assert_eq!(
            window.finish(1, u64::MAX),
            peak(u64::MAX, 1),
            "a rate past u64 saturates"
        );
    }
}
