//! The cold size a server sets ([`ColdSize`]): what a class with no estimate books, the floor
//! its memory keeps until five runs, and the history taking over from it. The hosts are the
//! farm's: 88 cores and 251 GiB of bookable memory, where a quarter of the host is 22 cores
//! and 62.75 GiB.

use std::collections::BTreeMap;
use std::time::Duration;

use js_jobs::{JobEnd, JobId, JobRecord};
use js_sizing::Bounds;
use js_types::{ManualClock, Resources, Shape, Timestamp};

use super::super::ColdSize;
use super::{
    GIB, JobSizes, MIB, Tables, at, booked, bounds_of, capacity, exited, host, needs, of_class,
    output, pass, queued, runs, size, submit,
};
use crate::engine::Settings;
use crate::place::SchedPlacer;

/// The farm's host: 88 cores and 251 GiB.
const HOST: (u64, u64) = (88_000, 251 * GIB);

/// The cold size this farm sets: two cores and 8 GiB.
const FARM: ColdSize = ColdSize {
    cpu_millis: Some(2000),
    memory_bytes: Some(8 * GIB),
};

/// The bounds of one farm host.
fn farm_bounds() -> Option<Bounds> {
    bounds_of([capacity(HOST.0, HOST.1)])
}

/// Sizes with `cold`, over one farm host, taught `jobs` (each booked as `booked`).
fn cold_taught(cold: ColdSize, jobs: &[JobRecord], booked: js_jobs::Booked) -> JobSizes {
    let mut sizes = JobSizes::with_cold(cold);
    sizes.set_bounds(farm_bounds());
    for job in jobs {
        assert!(sizes.observe(job, booked), "{} teaches a run", job.id);
    }
    sizes
}

/// Asserts what a class with no runs books under each setting on a farm host: with none, a
/// quarter of the host (22 cores and 62.75 GiB, today's booking); with both, the setting
/// (two cores and 8 GiB); with one, that one and a quarter of the host for the other; with a
/// setting above the host, the host's cores and memory. A job that names both dimensions is
/// booked as it names them under the setting too. Catches a setting that is not read, one
/// that replaces the other dimension's default, and one that books more than any host has
/// (which no host could place).
#[test]
fn a_class_with_no_runs_books_the_cold_size_the_server_sets() {
    let job = queued("cold", of_class("build", needs(0, 0)));
    let quarter_memory = 251 * GIB / 4;
    let cases = [
        (ColdSize::default(), (22_000, quarter_memory)),
        (FARM, (2000, 8 * GIB)),
        (
            ColdSize {
                cpu_millis: Some(2000),
                memory_bytes: None,
            },
            (2000, quarter_memory),
        ),
        (
            ColdSize {
                cpu_millis: None,
                memory_bytes: Some(8 * GIB),
            },
            (22_000, 8 * GIB),
        ),
        (
            ColdSize {
                cpu_millis: Some(200_000),
                memory_bytes: Some(400 * GIB),
            },
            HOST,
        ),
    ];
    for (cold, expected) in cases {
        let sizes = cold_taught(cold, &[], booked(0, 0));
        assert_eq!(size(&sizes, &job), expected, "{cold:?}");
    }
    let named = queued("named", of_class("build", needs(500, GIB)));
    let sizes = cold_taught(FARM, &[], booked(0, 0));
    assert_eq!(sizes.sized(&named, js_sizing::Day(0)), named);
}

/// Asserts that a cold size held at a host of less than it: on a host of one core and 4 GiB a
/// setting of two cores and 8 GiB books the whole host, and a quarter core is the least CPU
/// a cold class books on a host of a tenth of a core. Catches a cold booking no host of the
/// farm can take, and one of no CPU.
#[test]
fn the_cold_size_is_held_at_the_largest_host() {
    let job = queued("cold", of_class("build", needs(0, 0)));
    let mut small = JobSizes::with_cold(FARM);
    small.set_bounds(bounds_of([capacity(1000, 4 * GIB)]));
    assert_eq!(size(&small, &job), (1000, 4 * GIB));
    let mut tiny = JobSizes::with_cold(ColdSize::default());
    tiny.set_bounds(bounds_of([capacity(100, 4 * GIB)]));
    assert_eq!(size(&tiny, &job).0, 250);
}

/// Asserts that the history takes over from the cold size: runs booked two cores and 8 GiB
/// that used a core and a half and peaked at 2 GiB leave one run cold; two runs teach the CPU
/// (1500) while the memory stays at the 8 GiB setting (before five runs it may only rise from
/// the cold size, and it is the setting, not a quarter of the host, that it rises from); five
/// runs book what they used (`1.2 x 2 GiB + 128 MiB`, 2.75 GiB). Catches a floor still taken
/// from a quarter of the host (62.75 GiB until five runs), and a setting that outlives the
/// history.
#[test]
fn after_runs_the_history_takes_over_from_the_cold_size() {
    let tables = Tables::new();
    let end = || exited(0, 2 * GIB, Duration::from_secs(15));
    let jobs = runs(&tables, "build", 5, "build", booked(2000, 8 * GIB), end);
    let next = queued("next", of_class("build", needs(0, 0)));
    let at_cold = booked(2000, 8 * GIB);
    let expected = [
        (1, (2000, 8 * GIB)),
        (2, (1500, 8 * GIB)),
        (4, (1500, 8 * GIB)),
        (5, (1500, 11 * 256 * MIB)),
    ];
    for (taught, size_then) in expected {
        let sizes = cold_taught(FARM, &jobs[..taught], at_cold);
        assert_eq!(size(&sizes, &next), size_then, "after {taught} runs");
    }
}

/// Asserts that a run over the cold size raises the next booking of its class: a run stopped
/// for memory at the 8 GiB cold booking (a contained job's limit is its booking) makes the next
/// job book 16 GiB, twice the booking it died at; and two runs that peaked at 7 GiB (near the
/// cold size, not at it) make the next book 8.75 GiB (`1.2 x 7 GiB + 128 MiB`, rounded up to
/// 256 MiB). The CPU stays two cores. Catches a cold size that caps the class (it would be
/// killed at 8 GiB on every run) and a kill that does not raise it.
#[test]
fn a_run_over_the_cold_size_raises_the_next_booking() {
    let tables = Tables::new();
    let killed = || JobEnd::Memory {
        exit_code: None,
        output: output(8 * GIB, Duration::from_secs(5)),
    };
    let next = queued("next", of_class("big", needs(0, 0)));
    let kills = runs(&tables, "killed", 1, "big", booked(2000, 8 * GIB), killed);
    let sizes = cold_taught(FARM, &kills, booked(2000, 8 * GIB));
    assert_eq!(size(&sizes, &next), (2000, 16 * GIB));

    let near = || exited(0, 7 * GIB, Duration::from_secs(5));
    let heavy = runs(&tables, "heavy", 2, "heavy", booked(2000, 8 * GIB), near);
    let sizes = cold_taught(FARM, &heavy, booked(2000, 8 * GIB));
    let next = queued("next", of_class("heavy", needs(0, 0)));
    assert_eq!(size(&sizes, &next), (500, 35 * 256 * MIB));
}

/// A placer whose engine takes `cold`, over one farm host `a`, on a clock at zero.
fn farm_placer(cold: ColdSize) -> SchedPlacer {
    let clock = ManualClock::new(Timestamp::from_millis(0));
    let settings = Settings {
        cold,
        ..Settings::default()
    };
    let placer = SchedPlacer::new(settings, clock);
    let caps = BTreeMap::from([("arch".to_owned(), "x86_64".to_owned())]);
    let resources = Resources::new(HOST.0, HOST.1, 0, None).unwrap();
    placer
        .engine()
        .host_up(&host("a"), &Shape::new(resources, caps), at(0));
    placer
}

/// Asserts what one pass places on one farm host of 40 queued jobs of 40 classes that have
/// no runs and book nothing: with the setting of two cores and 8 GiB, each is booked that
/// and 31 are placed (31 x 8 GiB fits 251 GiB, 32 do not); without it, each is booked a
/// quarter of the host and 4 are placed. Catches an engine built without the setting, where
/// the farm runs four jobs a host until each target has history.
#[test]
fn a_pass_books_the_cold_size_the_engine_is_built_with() {
    for (cold, placed, each) in [
        (FARM, 31, booked(2000, 8 * GIB)),
        (ColdSize::default(), 4, booked(22_000, 251 * GIB / 4)),
    ] {
        let tables = Tables::new();
        let placer = farm_placer(cold);
        for n in 0..40 {
            submit(&tables, &format!("job-{n:02}"), &format!("target-{n:02}"));
        }
        let ids = pass(&placer, &tables);
        assert_eq!(ids.len(), placed, "{cold:?}");
        let first = JobId::parse("job-00").unwrap();
        let allocation = tables.jobs.get_allocation(&first).unwrap().unwrap();
        assert_eq!(allocation.booked, each, "{cold:?}");
    }
}
