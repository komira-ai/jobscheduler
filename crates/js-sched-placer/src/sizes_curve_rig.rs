//! What the tests of the live curve mode share: classes taught series that have a shape, and
//! queued jobs of them that book nothing. Every clock is the test's.

use std::collections::BTreeMap;
use std::time::Duration;

use js_jobs::{Booked, JobEnd, JobId, JobRecord, JobSpec, NewJob};
use js_types::{CpuSeries, Labels, MemorySeries, SIZE_CLASS_LABEL, SIZE_FALLBACK_LABEL};

use super::JobSizes;
use super::tests::{GIB, MIB, booked, exited, needs, of_class, output, ran_for};
use crate::engine::rig::Tables;

pub(crate) use super::profile::tests::capture_targets;

/// The seconds each point of the series of this module covers: a series of more than 128 s has
/// no more than 128 points.
pub(crate) const GRANULARITY: usize = 2;

/// The CPU a series of [`burst_at`] holds outside its burst: a quarter core (4 sixteenths).
pub(crate) const IDLE: u16 = 4;

/// A series of `len` seconds (a point every [`GRANULARITY`] seconds) at [`IDLE`] but for `burst`
/// sixteenths of a core in each of the four seconds from second `start`.
pub(crate) fn burst_at(start: usize, burst: u16, len: usize) -> Vec<u16> {
    let mut points = vec![IDLE; len / GRANULARITY];
    points[start / GRANULARITY..(start + 4) / GRANULARITY].fill(burst);
    points
}

/// What every run of this module booked: two cores and 4 GiB.
pub(crate) fn run_booking() -> Booked {
    booked(2000, 4 * GIB)
}

/// What a share of `cpu_millis` and `memory_bytes` books.
pub(crate) fn booking(cpu_millis: u64, memory_bytes: u64) -> Booked {
    booked(cpu_millis, memory_bytes)
}

/// The spec of a job of `class` that names `cpu_millis` and `memory_bytes`.
pub(crate) fn spec_of_class(class: &str, cpu_millis: u64, memory_bytes: u64) -> JobSpec {
    of_class(class, needs(cpu_millis, memory_bytes))
}

/// The queued job `id` of `class` that books nothing and names `fallback` as its fallback class,
/// recorded in `tables`.
pub(crate) fn queued_falling_back(
    tables: &Tables,
    id: &str,
    (class, fallback): (&str, &str),
) -> JobRecord {
    let mut spec = of_class(class, needs(0, 0));
    let labels = BTreeMap::from([
        (SIZE_CLASS_LABEL.to_owned(), class.to_owned()),
        (SIZE_FALLBACK_LABEL.to_owned(), fallback.to_owned()),
    ]);
    spec.labels = Labels::from_caller(&labels).unwrap();
    let new = NewJob {
        id: Some(JobId::parse(id).unwrap()),
        spec,
        priority: 0,
        client: None,
    };
    tables.jobs.insert_queued(new).unwrap().record
}

/// A series of `len` seconds at `level` sixteenths of a core until second `until`, then at
/// [`IDLE`].
pub(crate) fn busy_until(until: usize, level: u16, len: usize) -> Vec<u16> {
    let mut points = vec![IDLE; len / GRANULARITY];
    points[..until / GRANULARITY].fill(level);
    points
}

/// The queued job `id` of `class` that books nothing, recorded in `tables`.
pub(crate) fn queued_of(tables: &Tables, id: &str, class: &str) -> JobRecord {
    let new = NewJob {
        id: Some(JobId::parse(id).unwrap()),
        spec: of_class(class, needs(0, 0)),
        priority: 0,
        client: None,
    };
    tables.jobs.insert_queued(new).unwrap().record
}

/// A successful run of `class` recorded as `id` and freed in `tables`, whose CPU series is
/// `points` (a point every `granularity` seconds, a sixteenth of a core for a second being
/// 62 500 microseconds) and whose wall time is the series'. It reports no five-second peak, so
/// its class books the median of its runs' mean CPU.
pub(crate) fn series_run(
    tables: &Tables,
    (id, class): (&str, &str),
    (points, granularity): (&[u16], u32),
) -> JobRecord {
    let sixteenths: u64 = points.iter().map(|point| u64::from(*point)).sum();
    let secs = u64::try_from(points.len()).unwrap() * u64::from(granularity);
    let cpu = Duration::from_micros(sixteenths * u64::from(granularity) * 62_500);
    let mut output = output(GIB, cpu);
    let series = CpuSeries::new(granularity, points.to_vec(), secs * 1_000_000, false);
    output.cpu_series = Some(series.unwrap());
    output.usage.as_mut().unwrap().cpu_peak = None;
    let end = JobEnd::Exited {
        exit_code: 0,
        output,
    };
    let spec = of_class(class, needs(0, 0));
    let booked = booked(2000, 4 * GIB);
    let wall = Duration::from_secs(secs);
    ran_for(tables, (id, None), spec, booked, end, wall)
}

/// Teaches `sizes` `runs` runs of `class` whose CPU series is `points` at [`GRANULARITY`]
/// seconds a point; 24 runs score 16 (a profile scores none of its first eight), which is what a
/// class needs to book its curve.
pub(crate) fn teach(
    sizes: &mut JobSizes,
    tables: &Tables,
    (class, runs): (&str, usize),
    points: &[u16],
) {
    let granularity = u32::try_from(GRANULARITY).unwrap();
    for at in 0..runs {
        let id = format!("{class}-{at:02}");
        let job = series_run(tables, (&id, class), (points, granularity));
        assert!(sizes.observe(&job, booked(2000, 4 * GIB)), "{}", job.id);
    }
}

/// A run of `class` that succeeded and recorded no CPU series.
pub(crate) fn without_series(tables: &Tables, id: &str, class: &str) -> JobRecord {
    let end = exited(0, GIB, Duration::from_secs(10));
    let spec = of_class(class, needs(0, 0));
    ran_for(
        tables,
        (id, None),
        spec,
        booked(2000, 4 * GIB),
        end,
        Duration::from_secs(10),
    )
}

/// A run of `class` that exited with code 1, with no series.
pub(crate) fn failed(tables: &Tables, id: &str, class: &str) -> JobRecord {
    let end = exited(1, GIB, Duration::from_secs(10));
    let spec = of_class(class, needs(0, 0));
    ran_for(
        tables,
        (id, None),
        spec,
        booked(2000, 4 * GIB),
        end,
        Duration::from_secs(10),
    )
}

/// A successful run of `class` recorded as `id` and freed in `tables`, whose memory series is
/// `points` MiB (one second a point) and whose wall time and peak are the series': it records
/// no CPU series.
pub(crate) fn memory_run(tables: &Tables, (id, class): (&str, &str), points: &[u32]) -> JobRecord {
    let secs = u64::try_from(points.len()).unwrap();
    let peak = u64::from(points.iter().copied().max().unwrap()) * MIB;
    let mut output = output(peak, Duration::from_secs(secs));
    let series = MemorySeries::new(1, points.to_vec(), secs * 1_000_000, false);
    output.memory_series = Some(series.unwrap());
    let end = JobEnd::Exited {
        exit_code: 0,
        output,
    };
    let spec = of_class(class, needs(0, 0));
    let wall = Duration::from_secs(secs);
    ran_for(tables, (id, None), spec, booked(2000, 4 * GIB), end, wall)
}

/// Teaches `sizes` `runs` runs of `class` whose memory series is `points` MiB, one second a
/// point; 24 runs score 16, which is what a class needs to have a memory curve.
pub(crate) fn teach_memory(
    sizes: &mut JobSizes,
    tables: &Tables,
    (class, runs): (&str, usize),
    points: &[u32],
) {
    for at in 0..runs {
        let id = format!("{class}-{at:02}");
        let job = memory_run(tables, (&id, class), points);
        assert!(sizes.observe(&job, booked(2000, 4 * GIB)), "{}", job.id);
    }
}
