//! A run's memory second by second ([`MemoryRecorder`]), sampled at the same polls as its CPU
//! ([`super::cpu::SeriesRecorder`]), on the agent's monotonic clock. What the job's result
//! reports as [`JobResult::memory_series`](js_agent_wire::JobResult::memory_series).
//!
//! What a sample measures is the runtime's: on Linux the non-file memory of the lease's group
//! (`crate::crun`'s `memory_series::nonfile_of`: `memory.current - (file - shmem)`, which the
//! kernel cannot drop, and none of the page cache of the files the job read); on a Mac the
//! physical footprint of the run's processes together ([`crate::mac_usage`]), which counts no
//! file page either.
//!
//! The series holds at most [`MAX_POINTS`] points and one granularity, the seconds each point
//! covers. It starts at one second a point. When a run outlasts its points, neighbouring
//! points are combined in pairs by their maximum and the granularity doubles: 128 s at 1 s a
//! point, 256 s at 2 s, and so on. A sample is a level, not a rate: it goes into the point of
//! its time, which keeps the largest it was given, and a point with no sample of its own (a
//! pause of the agent's polls) holds the last sample before it.

use js_agent_wire::{MEMORY_SERIES_BYTES_PER_POINT, MEMORY_SERIES_MAX_POINTS, MemorySeries};

/// The most points a series holds.
pub(crate) const MAX_POINTS: usize = MEMORY_SERIES_MAX_POINTS;
/// One second, in microseconds.
const SECOND_USEC: u64 = 1_000_000;

/// The memory of one run per point of its series. Pure: the caller reads the clock and
/// what the run holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MemoryRecorder {
    /// The time the series starts at, in microseconds; meaningful once there is a sample for
    /// an adopted run.
    start: u64,
    /// The time of the last sample, in microseconds, and the bytes it read.
    last: Option<(u64, u64)>,
    /// Whether the series has a start: a run this process made has one from its creation, an
    /// adopted one from its first sample.
    started: bool,
    /// The time the run was seen to end, in microseconds ([`MemoryRecorder::end_at`]).
    ended: Option<u64>,
    /// The seconds each point covers: 1, then doubled each time the points are combined.
    granularity: u64,
    /// The most bytes of each point.
    points: Vec<u64>,
    /// Whether the series misses the start of the run (an adopted run).
    partial: bool,
}

impl MemoryRecorder {
    /// A series of a run made at `at` microseconds.
    pub(crate) fn starting(at: u64) -> Self {
        Self {
            start: at,
            last: None,
            started: true,
            ended: None,
            granularity: 1,
            points: Vec::new(),
            partial: false,
        }
    }

    /// A series of a run this process did not start (adopted after a restart of the agent),
    /// marked partial: it starts at its first sample. Only the crun runtime adopts a run (a
    /// Mac's native runs end with the agent), so only Linux has it.
    #[cfg(target_os = "linux")]
    pub(crate) fn adopted() -> Self {
        Self {
            start: 0,
            last: None,
            started: false,
            ended: None,
            granularity: 1,
            points: Vec::new(),
            partial: true,
        }
    }

    /// Records that the run was seen to end at `at` microseconds: the run the series reports
    /// ends there.
    pub(crate) fn end_at(&mut self, at: u64) {
        self.ended = Some(at);
    }

    /// Takes the sample `bytes` at `at` microseconds into the point of its time, which keeps
    /// the largest sample it was given. The points between the last sample's and this one's
    /// hold the last sample. A sample earlier than the last one, or before the start, adds
    /// nothing.
    pub(crate) fn observe(&mut self, at: u64, bytes: u64) {
        if !self.started {
            self.start = at;
            self.started = true;
        }
        if at < self.start || self.last.is_some_and(|(last, _)| at < last) {
            return;
        }
        let offset = at - self.start;
        let index = loop {
            let index = offset / (self.granularity * SECOND_USEC);
            match usize::try_from(index) {
                Ok(index) if index < MAX_POINTS => break index,
                _ => self.combine(),
            }
        };
        if self.points.len() <= index {
            // The spans since the last sample's hold it; this one starts with its own.
            let held = self.last.map_or(0, |(_, held)| held);
            self.points.resize(index, held);
            self.points.push(bytes);
        } else {
            self.points[index] = self.points[index].max(bytes);
        }
        self.last = Some((at, bytes));
    }

    /// The series: each point the most memory of its span in MiB rounded up, over
    /// the run from its start to the time it was seen to end (or its last sample, or `at`,
    /// whichever is first known). `None` for a run with no sample or no time.
    pub(crate) fn finish(self, at: u64) -> Option<MemorySeries> {
        let (last, _) = self.last?;
        let end = self.ended.unwrap_or(at).max(last);
        let run_usec = end - self.start;
        if run_usec == 0 {
            return None;
        }
        Some(MemorySeries {
            granularity_secs: u32::try_from(self.granularity).unwrap_or(u32::MAX),
            points: self.points.iter().map(|&bytes| to_point(bytes)).collect(),
            run_usec,
            partial: self.partial,
        })
    }

    /// Combines neighbouring points in pairs by their maximum and doubles the granularity.
    fn combine(&mut self) {
        self.points = self
            .points
            .chunks(2)
            .map(|pair| pair.iter().copied().max().unwrap_or(0))
            .collect();
        self.granularity *= 2;
    }
}

/// `bytes` as a point: MiB rounded up, saturating.
fn to_point(bytes: u64) -> u32 {
    let mib = bytes.div_ceil(MEMORY_SERIES_BYTES_PER_POINT);
    u32::try_from(mib).unwrap_or(u32::MAX)
}

#[cfg(test)]
#[path = "memory_tests.rs"]
mod tests;
