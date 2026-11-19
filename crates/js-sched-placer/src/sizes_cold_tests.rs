//! The cold size a server sets ([`ColdSize`]): what a class with no estimate books, the floor
//! its memory keeps until five runs, and the history taking over from it. The hosts are the
//! farm's: 88 cores and 251 GiB of bookable memory, where the default cold size is 4 cores and
//! 16 GiB.

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
    cpu_millis: 2000,
    memory_bytes: 8 * GIB,
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

/// Asserts what a class with no runs books under each setting on a farm host: with none, the
/// default of 4 cores and 16 GiB (not a quarter of the host, 22 cores and 62.75 GiB); with
/// both, the setting (two cores and 8 GiB); with one, that one and the default for the other;
/// with a setting above the host, the host's cores and memory. A job that names both
/// dimensions is booked as it names them under the setting too. Catches a default taken from
/// the host's size, a setting that is not read, one that replaces the other dimension's
/// default, and one that books more than any host has (which no host could place).
#[test]
fn a_class_with_no_runs_books_the_cold_size_the_server_sets() {
    let job = queued("cold", of_class("build", needs(0, 0)));
    let cases = [
        (ColdSize::default(), (4000, 16 * GIB)),
        (FARM, (2000, 8 * GIB)),
        (
            ColdSize {
                cpu_millis: 2000,
                ..ColdSize::default()
            },
            (2000, 16 * GIB),
        ),
        (
            ColdSize {
                memory_bytes: 8 * GIB,
                ..ColdSize::default()
            },
            (4000, 8 * GIB),
        ),
        (
            ColdSize {
                cpu_millis: 200_000,
                memory_bytes: 400 * GIB,
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
/// setting of two cores and 8 GiB books the whole host, and so does the default of 4 cores and
/// 16 GiB on a host of two cores and 8 GiB; a host of exactly the default books the default;
/// a host under the default in one dimension only holds that one (two cores and 251 GiB books
/// two cores and 16 GiB, 88 cores and 8 GiB books 4 cores and 8 GiB); and a quarter core is
/// the least CPU a cold class books on a host of a tenth of a core. Catches a cold booking no
/// host of the farm can take, one dimension held at the other's bound, and one of no CPU.
#[test]
fn the_cold_size_is_held_at_the_largest_host() {
    let job = queued("cold", of_class("build", needs(0, 0)));
    let mut small = JobSizes::with_cold(FARM);
    small.set_bounds(bounds_of([capacity(1000, 4 * GIB)]));
    assert_eq!(size(&small, &job), (1000, 4 * GIB));
    let mut under_default = JobSizes::new();
    under_default.set_bounds(bounds_of([capacity(2000, 8 * GIB)]));
    assert_eq!(size(&under_default, &job), (2000, 8 * GIB));
    let mut at_default = JobSizes::new();
    at_default.set_bounds(bounds_of([capacity(4000, 16 * GIB)]));
    assert_eq!(size(&at_default, &job), (4000, 16 * GIB));
    for (host, expected) in [
        ((2000, 251 * GIB), (2000, 16 * GIB)),
        ((88_000, 8 * GIB), (4000, 8 * GIB)),
    ] {
        let mut sizes = JobSizes::new();
        sizes.set_bounds(bounds_of([capacity(host.0, host.1)]));
        assert_eq!(size(&sizes, &job), expected, "{host:?}");
    }
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
/// and 31 are placed (31 x 8 GiB fits 251 GiB, 32 do not); without it, each is booked the
/// default of 4 cores and 16 GiB and 15 are placed (15 x 16 GiB fits 251 GiB, 16 do not).
/// Catches an engine built without the setting, and a default taken from the host's size,
/// where the farm runs four jobs a host until each target has history.
#[test]
fn a_pass_books_the_cold_size_the_engine_is_built_with() {
    for (cold, placed, each) in [
        (FARM, 31, booked(2000, 8 * GIB)),
        (ColdSize::default(), 15, booked(4000, 16 * GIB)),
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

/// Asserts that a class's kill floor wins over a cold memory below it: with the setting at
/// 4 GiB, a class killed at an 8 GiB booking books 16 GiB next, and a class with no runs
/// books 4 GiB. And that the cold size follows the largest host the engine knows: a setting of
/// 8 GiB held at a 4 GiB host books 8 GiB once a 251 GiB host is known. And under the default
/// (16 GiB), a class killed at a 16 GiB booking books 32 GiB next (and the estimator's
/// unlearned two cores, as any class with an estimate and no CPU sample). Catches a setting or the
/// default overriding the floor a kill left, and a held value kept after a larger host appears.
#[test]
fn the_kill_floor_and_a_larger_host_win_over_the_cold_memory() {
    let tables = Tables::new();
    let killed = || JobEnd::Memory {
        exit_code: None,
        output: output(8 * GIB, Duration::from_secs(5)),
    };
    let kills = runs(&tables, "killed", 1, "big", booked(2000, 8 * GIB), killed);
    let low = ColdSize {
        cpu_millis: 2000,
        memory_bytes: 4 * GIB,
    };
    let sizes = cold_taught(low, &kills, booked(2000, 8 * GIB));
    let big = queued("big", of_class("big", needs(0, 0)));
    assert_eq!(size(&sizes, &big), (2000, 16 * GIB));
    let fresh = queued("fresh", of_class("fresh", needs(0, 0)));
    assert_eq!(size(&sizes, &fresh), (2000, 4 * GIB));

    let mut growing = JobSizes::with_cold(FARM);
    growing.set_bounds(bounds_of([capacity(1000, 4 * GIB)]));
    assert_eq!(size(&growing, &fresh), (1000, 4 * GIB));
    growing.set_bounds(farm_bounds());
    assert_eq!(size(&growing, &fresh), (2000, 8 * GIB));

    let killed_at_default = || JobEnd::Memory {
        exit_code: None,
        output: output(16 * GIB, Duration::from_secs(5)),
    };
    let at_default = booked(4000, 16 * GIB);
    let kills = runs(
        &tables,
        "killed16",
        1,
        "big16",
        at_default,
        killed_at_default,
    );
    let sizes = cold_taught(ColdSize::default(), &kills, at_default);
    let big = queued("big16", of_class("big16", needs(0, 0)));
    assert_eq!(size(&sizes, &big), (2000, 32 * GIB));
}

/// Asserts what the default books on a farm whose largest host has 16 GiB of bookable memory
/// (8 cores), which the default reaches: a class with no runs books 4 cores and the whole
/// 16 GiB, and so does a class with two runs that peaked at 1 GiB (before five runs its memory
/// may only rise from the cold memory, which is the host's); five such runs book what they
/// used (`1.2 x 1 GiB + 128 MiB`, 1.5 GiB). Catches a default that books more than the host,
/// and a young class booked below the cold memory.
#[test]
fn on_a_host_of_the_default_memory_a_young_class_books_the_host() {
    let tables = Tables::new();
    let end = || exited(0, GIB, Duration::from_secs(10));
    let at_default = booked(4000, 16 * GIB);
    let jobs = runs(&tables, "small", 5, "small", at_default, end);
    let next = queued("next", of_class("small", needs(0, 0)));
    for (taught, expected) in [
        (0, (4000, 16 * GIB)),
        (2, (4000, 16 * GIB)),
        (5, (1000, 6 * 256 * MIB)),
    ] {
        let mut sizes = JobSizes::new();
        sizes.set_bounds(bounds_of([capacity(8000, 16 * GIB)]));
        for job in &jobs[..taught] {
            assert!(sizes.observe(job, at_default), "{} teaches a run", job.id);
        }
        assert_eq!(size(&sizes, &next), expected, "after {taught} runs");
    }
}
