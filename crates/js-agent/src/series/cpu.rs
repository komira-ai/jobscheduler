//! A run's CPU second by second ([`SeriesRecorder`]), from samples of the CPU its processes
//! have used (on Linux the `usage_usec` of its lease's `cpu.stat`, the samples the crun
//! runtime's CPU peak also takes; on a Mac the CPU time of the run's processes,
//! [`crate::mac_usage`]), on the agent's monotonic clock. What the job's result reports as
//! [`JobResult::cpu_series`](js_agent_wire::JobResult::cpu_series) (the design of
//! komira-ai/jobscheduler#1033, section 3).
//!
//! The series holds at most [`MAX_POINTS`] points and one granularity, the seconds each point
//! covers. It starts at one second a point. When a run outlasts its points, neighbouring
//! points are combined in pairs by their mean and the granularity doubles: 128 s at 1 s a
//! point, 256 s at 2 s, 512 s at 4 s, and so on. CPU between two samples is spread evenly over
//! the time between them, so a gap of k seconds spreads its CPU over those k seconds.

use js_agent_wire::{CPU_SERIES_MAX_POINTS, CPU_SERIES_POINTS_PER_CORE, CpuSeries};

/// The most points a series holds.
pub(crate) const MAX_POINTS: usize = CPU_SERIES_MAX_POINTS;
/// One second, in microseconds.
const SECOND_USEC: u64 = 1_000_000;

/// The CPU of one run per point of its series. Pure: the caller reads the clock and the
/// counter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SeriesRecorder {
    /// The time the series starts at, in microseconds; meaningful once there is a sample.
    start: u64,
    /// The last sample: `(time, usage)`, both in microseconds; none for an adopted run until
    /// its first sample.
    last: Option<(u64, u64)>,
    /// The time the run was seen to end, in microseconds ([`SeriesRecorder::end_at`]).
    ended: Option<u64>,
    /// The seconds each point covers: 1, then doubled each time the points are combined.
    granularity: u64,
    /// The CPU microseconds in each point, the last one still open. Kept as sums so that
    /// combining two points is exact; their means are taken only when the series is sent.
    points: Vec<u64>,
    /// Whether the series misses part of the run's CPU detail (an adopted run).
    partial: bool,
}

impl SeriesRecorder {
    /// A series that starts at `at` microseconds with `usage` CPU microseconds already used:
    /// a run made at `at` starts from zero.
    pub(crate) fn starting(at: u64, usage: u64) -> Self {
        Self {
            start: at,
            last: Some((at, usage)),
            ended: None,
            granularity: 1,
            points: Vec::new(),
            partial: false,
        }
    }

    /// A series of a run this process did not start (adopted after a restart of the agent),
    /// marked partial: what the run did before it is not in the series. It starts at its
    /// first sample and counts what the run used from there.
    pub(crate) fn adopted() -> Self {
        Self {
            start: 0,
            last: None,
            ended: None,
            granularity: 1,
            points: Vec::new(),
            partial: true,
        }
    }

    /// Records that the run was seen to end at `at` microseconds: [`SeriesRecorder::finish`]
    /// then ends the series there, not at the later time it is given.
    pub(crate) fn end_at(&mut self, at: u64) {
        self.ended = Some(at);
    }

    /// Takes the sample `usage` CPU microseconds at `at` microseconds, spreading the CPU
    /// used since the last sample evenly over the time since it. A sample no later than the
    /// last one adds nothing; a counter that fell counts as no use. The first sample of a
    /// series with none (an adopted run's) sets its start and adds nothing.
    pub(crate) fn observe(&mut self, at: u64, usage: u64) {
        let Some((from, used)) = self.last else {
            self.start = at;
            self.last = Some((at, usage));
            return;
        };
        if at <= from {
            return;
        }
        let spent = usage.saturating_sub(used);
        // Offsets from the start of the series, in microseconds.
        let (begin, end) = (from - self.start, at - self.start);
        let span = end - begin;
        let mut cursor = begin;
        let mut given = 0;
        while cursor < end {
            let width = self.granularity * SECOND_USEC;
            let index = usize::try_from(cursor / width).unwrap_or(usize::MAX);
            if index >= MAX_POINTS {
                self.combine();
                continue;
            }
            let until = end.min((cursor / width + 1) * width);
            // The share of `spent` up to `until`, so that the shares add up to `spent`.
            let share = mul_div(spent, until - begin, span);
            if self.points.len() <= index {
                self.points.resize(index + 1, 0);
            }
            self.points[index] += share - given;
            given = share;
            cursor = until;
        }
        self.last = Some((at, usage));
    }

    /// The series, with its last sample `usage` at `at`, or at the time the run was seen to
    /// end when there is one ([`SeriesRecorder::end_at`]): each point the mean CPU over the
    /// time it covers, the last point over the part of it the run lived. The CPU since the
    /// sample before is spread over the time since it. `None` for a run with no time between
    /// its start and its end.
    pub(crate) fn finish(mut self, at: u64, usage: u64) -> Option<CpuSeries> {
        self.observe(self.ended.unwrap_or(at), usage);
        let run_usec = self.last.map_or(0, |(last, _)| last - self.start);
        if run_usec == 0 {
            return None;
        }
        let width = self.granularity * SECOND_USEC;
        let count = self.points.len();
        let points = self
            .points
            .iter()
            .enumerate()
            .map(|(index, &used)| {
                let covered = if index + 1 == count {
                    run_usec - width * (count as u64 - 1)
                } else {
                    width
                };
                to_point(used, covered)
            })
            .collect();
        Some(CpuSeries {
            granularity_secs: u32::try_from(self.granularity).unwrap_or(u32::MAX),
            points,
            run_usec,
            partial: self.partial,
        })
    }

    /// Combines neighbouring points in pairs and doubles the granularity. The points hold
    /// sums, so a combined point's mean is the mean of the two it replaces.
    fn combine(&mut self) {
        self.points = self
            .points
            .chunks(2)
            .map(|pair| pair.iter().sum())
            .collect();
        self.granularity *= 2;
    }
}

/// `used` CPU microseconds over `covered` microseconds, as a point: the mean in
/// [`CPU_SERIES_POINTS_PER_CORE`]ths of a core, rounded to the nearest, saturating.
fn to_point(used: u64, covered: u64) -> u16 {
    let per_core = u128::from(CPU_SERIES_POINTS_PER_CORE);
    let covered = u128::from(covered);
    let point = (u128::from(used) * per_core + covered / 2) / covered;
    u16::try_from(point).unwrap_or(u16::MAX)
}

/// `value * part / whole`, exact for any `u64`s with `part <= whole` and `whole > 0`.
fn mul_div(value: u64, part: u64, whole: u64) -> u64 {
    let share = u128::from(value) * u128::from(part) / u128::from(whole);
    u64::try_from(share).unwrap_or(u64::MAX)
}

#[cfg(test)]
#[path = "cpu_tests.rs"]
mod tests;
