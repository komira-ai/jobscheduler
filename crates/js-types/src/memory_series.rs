//! A job's memory over its run, second by second and coarsened as it ran ([`MemorySeries`]),
//! as the server keeps it: the form a host reports in a job's result.

/// The most points a [`MemorySeries`] holds.
pub const MEMORY_SERIES_MAX_POINTS: usize = 128;

/// The bytes of one unit of a [`MemorySeries`] point: one MiB.
pub const MEMORY_SERIES_BYTES_PER_POINT: u64 = 1 << 20;

/// A job's non-file memory over its run: at most [`MEMORY_SERIES_MAX_POINTS`] points, each the
/// most the job held over `granularity_secs` seconds of its run, in order from its start, in
/// MiB rounded up ([`MEMORY_SERIES_BYTES_PER_POINT`]).
///
/// Non-file memory is what the job's control group holds less its file pages that are not
/// shared memory (`memory.current - (file - shmem)` of its `memory.stat`): its anonymous and
/// kernel memory and its shared memory and tmpfs pages, which the kernel cannot drop, and none
/// of the page cache of the files it read, which it can.
///
/// The series starts at one second a point; each time a run outlasts its points, neighbouring
/// points are combined in pairs by their maximum and the granularity doubles, so the largest
/// point is the most the job was seen to hold at any sample.
///
/// A series is well formed when its granularity is a power of two that fits 16 bits and it has
/// at most [`MEMORY_SERIES_MAX_POINTS`] points ([`MemorySeries::new`]); the server keeps only
/// well-formed ones.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemorySeries {
    /// The seconds each point covers: 1, 2, 4, 8 and so on.
    pub granularity_secs: u32,
    /// The most non-file memory of each span of the run, in MiB rounded up.
    pub points: Vec<u32>,
    /// The job's run as its host measured it, in microseconds.
    pub run_usec: u64,
    /// Whether the series misses the start of the run (a run its host adopted after a restart
    /// of its own).
    pub partial: bool,
}

impl MemorySeries {
    /// The series of `points` at `granularity_secs` seconds a point over a run of `run_usec`
    /// microseconds, when it is well formed: a granularity that is a power of two and fits 16
    /// bits, and at most [`MEMORY_SERIES_MAX_POINTS`] points. `None` otherwise.
    #[must_use]
    pub fn new(
        granularity_secs: u32,
        points: Vec<u32>,
        run_usec: u64,
        partial: bool,
    ) -> Option<Self> {
        let series = Self {
            granularity_secs,
            points,
            run_usec,
            partial,
        };
        series.is_well_formed().then_some(series)
    }

    /// Whether the series is well formed: a granularity that is a power of two and fits 16
    /// bits, and at most [`MEMORY_SERIES_MAX_POINTS`] points.
    #[must_use]
    pub fn is_well_formed(&self) -> bool {
        u16::try_from(self.granularity_secs).is_ok_and(u16::is_power_of_two)
            && self.points.len() <= MEMORY_SERIES_MAX_POINTS
    }

    /// The most non-file memory the series saw, in bytes: its largest point times
    /// [`MEMORY_SERIES_BYTES_PER_POINT`]. `None` for a series with no point.
    #[must_use]
    pub fn peak_bytes(&self) -> Option<u64> {
        let most = self.points.iter().copied().max()?;
        Some(u64::from(most).saturating_mul(MEMORY_SERIES_BYTES_PER_POINT))
    }
}
