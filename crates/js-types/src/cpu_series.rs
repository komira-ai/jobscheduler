//! A job's CPU over its run, second by second and coarsened as it ran ([`CpuSeries`]), as the
//! server keeps it: the form a host reports in a job's result and the form the usage history
//! and the usage listing hold.

/// The most points a [`CpuSeries`] holds.
pub const CPU_SERIES_MAX_POINTS: usize = 128;

/// A job's CPU over its run: at most [`CPU_SERIES_MAX_POINTS`] points, each the mean CPU of the
/// job over `granularity_secs` seconds of its run, in order from its start, in sixteenths of a
/// core (16 is one core busy for the whole span).
///
/// The series starts at one second a point; each time a run outlasts its points, neighbouring
/// points are combined in pairs by their mean and the granularity doubles. The last point
/// covers what is left of the run after the others. The server keeps a series as the host
/// reported it and reads nothing into it.
///
/// A series is well formed when its granularity is a power of two that fits 16 bits and it has
/// at most [`CPU_SERIES_MAX_POINTS`] points ([`CpuSeries::new`]); the server stores and lists
/// only well-formed ones.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CpuSeries {
    /// The seconds each point covers: 1, 2, 4, 8 and so on.
    pub granularity_secs: u32,
    /// The mean CPU of each span of the run, in sixteenths of a core.
    pub points: Vec<u16>,
    /// The job's run as its host measured it, in microseconds.
    pub run_usec: u64,
    /// Whether the series misses the start of the run (a run its host adopted after a restart
    /// of its own).
    pub partial: bool,
}

impl CpuSeries {
    /// The series of `points` at `granularity_secs` seconds a point over a run of `run_usec`
    /// microseconds, when it is well formed: a granularity that is a power of two and fits 16
    /// bits, and at most [`CPU_SERIES_MAX_POINTS`] points. `None` otherwise.
    #[must_use]
    pub fn new(
        granularity_secs: u32,
        points: Vec<u16>,
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
    /// bits, and at most [`CPU_SERIES_MAX_POINTS`] points.
    #[must_use]
    pub fn is_well_formed(&self) -> bool {
        u16::try_from(self.granularity_secs).is_ok_and(u16::is_power_of_two)
            && self.points.len() <= CPU_SERIES_MAX_POINTS
    }
}
