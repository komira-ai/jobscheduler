//! The cold size a server sets ([`ColdSize`]): what a class books when neither it, its fallback
//! class nor its client has a run, and the history taking over from it with the first run.
//! The hosts are the farm's: 88 cores and 251 GiB of bookable memory, where the default cold
//! size is 22 cores (held at a quarter of the host's cores) and 4 GiB.

use std::collections::BTreeMap;
use std::time::Duration;

use js_jobs::{JobEnd, JobId, JobRecord};
use js_sizing::Bounds;
use js_types::{ManualClock, Resources, Shape, Timestamp};

use super::super::{ColdMemoryOn, ColdSize};
use super::{
    GIB, JobSizes, MIB, Tables, at, booked, bounds_of, capacity, exited, host, needs, of_class,
    output, pass, queued, queued_by, runs, size, submit,
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
/// default of 22 cores and 4 GiB (22 cores is a quarter of the host's cores, but 4 GiB is not
/// a quarter of its memory, 62.75 GiB); with both, the setting (two cores and 8 GiB); with
/// one, that one and the default for the other; with a setting above the host, a quarter of
/// the host's cores (22) and the host's memory. A job that names both
/// dimensions is booked as it names them under the setting too. Catches a default taken from
/// the host's size, a setting that is not read, one that replaces the other dimension's
/// default, and one that books more than any host has (which no host could place).
#[test]
fn a_class_with_no_runs_books_the_cold_size_the_server_sets() {
    let job = queued("cold", of_class("build", needs(0, 0)));
    let cases = [
        (ColdSize::default(), (22_000, 4 * GIB)),
        (FARM, (2000, 8 * GIB)),
        (
            ColdSize {
                cpu_millis: 2000,
                ..ColdSize::default()
            },
            (2000, 4 * GIB),
        ),
        (
            ColdSize {
                memory_bytes: 8 * GIB,
                ..ColdSize::default()
            },
            (22_000, 8 * GIB),
        ),
        (
            ColdSize {
                cpu_millis: 200_000,
                memory_bytes: 400 * GIB,
            },
            (22_000, 251 * GIB),
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

/// Asserts that a cold size is held at a host of less than it: its CPU at a quarter of the
/// host's cores and its memory at the host's. On a host of one core and 4 GiB a setting of two
/// cores and 8 GiB books a quarter core and 4 GiB, and the default of 22 cores and 4 GiB on a
/// host of two cores and 2 GiB books half a core and 2 GiB; a host of four cores and 4 GiB
/// books one core and 4 GiB; a host under the default in one dimension only holds that one
/// (two cores and 251 GiB books half a core and 4 GiB, 88 cores and 2 GiB books 22 cores and
/// 2 GiB); and a quarter core is the least CPU a cold class books on a host of a tenth of a
/// core. Catches a cold booking no host of the farm can take, one dimension held at the
/// other's bound, and one of no CPU.
#[test]
fn the_cold_size_is_held_at_the_largest_host() {
    let job = queued("cold", of_class("build", needs(0, 0)));
    let mut small = JobSizes::with_cold(FARM);
    small.set_bounds(bounds_of([capacity(1000, 4 * GIB)]));
    assert_eq!(size(&small, &job), (250, 4 * GIB));
    let mut under_default = JobSizes::new();
    under_default.set_bounds(bounds_of([capacity(2000, 2 * GIB)]));
    assert_eq!(size(&under_default, &job), (500, 2 * GIB));
    let mut at_default = JobSizes::new();
    at_default.set_bounds(bounds_of([capacity(4000, 4 * GIB)]));
    assert_eq!(size(&at_default, &job), (1000, 4 * GIB));
    for (host, expected) in [
        ((2000, 251 * GIB), (500, 4 * GIB)),
        ((88_000, 2 * GIB), (22_000, 2 * GIB)),
    ] {
        let mut sizes = JobSizes::new();
        sizes.set_bounds(bounds_of([capacity(host.0, host.1)]));
        assert_eq!(size(&sizes, &job), expected, "{host:?}");
    }
    let mut tiny = JobSizes::with_cold(ColdSize::default());
    tiny.set_bounds(bounds_of([capacity(100, 4 * GIB)]));
    assert_eq!(size(&tiny, &job).0, 250);
}

/// Asserts that the history takes over from the cold size with the first run: a class with no
/// run books the 8 GiB setting; runs booked two cores and 8 GiB that used a core and a half and
/// peaked at 2 GiB teach the class its CPU (1500) and its memory (`1.2 x 2 GiB + 128 MiB`,
/// 2.75 GiB, below the setting) from even one run, whatever their number up to and past five.
/// Catches a young class still held at the setting (or at a quarter of the host, 62.75 GiB),
/// and a setting that outlives the history.
#[test]
fn the_first_run_takes_the_history_over_from_the_cold_size() {
    let tables = Tables::new();
    let end = || exited(0, 2 * GIB, Duration::from_secs(15));
    let jobs = runs(&tables, "build", 5, "build", booked(2000, 8 * GIB), end);
    let next = queued("next", of_class("build", needs(0, 0)));
    let at_cold = booked(2000, 8 * GIB);
    let expected = [
        (0, (2000, 8 * GIB)),
        (1, (1500, 11 * 256 * MIB)),
        (2, (1500, 11 * 256 * MIB)),
        (4, (1500, 11 * 256 * MIB)),
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
/// default of 22 cores and 4 GiB and 4 are placed (4 x 22 cores fit the 88 cores at the CPU
/// slack of 1.1, 5 do not). Catches an engine built without the setting, and a default taken
/// from the host's size, and a booking that is not held at a quarter of the host.
#[test]
fn a_pass_books_the_cold_size_the_engine_is_built_with() {
    for (cold, placed, each) in [
        (FARM, 31, booked(2000, 8 * GIB)),
        (ColdSize::default(), 4, booked(22_000, 4 * GIB)),
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
/// 4 GiB, a class killed at an 8 GiB booking books 16 GiB next, while another class of the same
/// client books the cold 4 GiB (a kill raises its own class's floor, not every class of its
/// client's), as does a class of another client. And that the cold size follows the largest host the engine knows: a setting of
/// 8 GiB held at a 4 GiB host (of one core, so a quarter core) books 8 GiB and two cores once a
/// 251 GiB host of 88 cores is known. And under the default
/// (4 GiB), a class killed at a 16 GiB booking books 32 GiB next, with the cold 22 cores (a
/// class with no CPU sample books the cold CPU). Catches a setting or the default overriding
/// the floor a kill left, a kill that raises every class of its client, and a held value kept
/// after a larger host appears.
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
    let sibling = queued("sibling", of_class("fresh", needs(0, 0)));
    assert_eq!(
        size(&sizes, &sibling),
        (2000, 4 * GIB),
        "no floor from the client"
    );
    let fresh = queued_by(Some("fresh"), "fresh", of_class("fresh", needs(0, 0)));
    assert_eq!(size(&sizes, &fresh), (2000, 4 * GIB));

    let mut growing = JobSizes::with_cold(FARM);
    growing.set_bounds(bounds_of([capacity(1000, 4 * GIB)]));
    assert_eq!(size(&growing, &fresh), (250, 4 * GIB));
    growing.set_bounds(farm_bounds());
    assert_eq!(size(&growing, &fresh), (2000, 8 * GIB));

    let killed_at_default = || JobEnd::Memory {
        exit_code: None,
        output: output(16 * GIB, Duration::from_secs(5)),
    };
    let at_default = booked(22_000, 16 * GIB);
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
    assert_eq!(size(&sizes, &big), (22_000, 32 * GIB));
}

/// Asserts what the default books on a farm whose largest host has 4 GiB of bookable memory
/// (8 cores), which the default reaches: a class with no runs books 2 cores (a quarter of the host) and the whole
/// 4 GiB; a class with two runs that peaked at 1 GiB, or five, books what they used (their
/// CPU, a core, and `1.2 x 1 GiB + 128 MiB`, 1.5 GiB), not the cold memory that is the host's.
/// Catches a default that books more than the host, and a young class held at the cold memory.
#[test]
fn on_a_host_of_the_default_memory_a_cold_class_books_the_host_and_a_young_class_what_it_used() {
    let tables = Tables::new();
    let end = || exited(0, GIB, Duration::from_secs(10));
    let at_default = booked(2000, 4 * GIB);
    let jobs = runs(&tables, "small", 5, "small", at_default, end);
    let next = queued("next", of_class("small", needs(0, 0)));
    for (taught, expected) in [
        (0, (2000, 4 * GIB)),
        (2, (1000, 6 * 256 * MIB)),
        (5, (1000, 6 * 256 * MIB)),
    ] {
        let mut sizes = JobSizes::new();
        sizes.set_bounds(bounds_of([capacity(8000, 4 * GIB)]));
        for job in &jobs[..taught] {
            assert!(sizes.observe(job, at_default), "{} teaches a run", job.id);
        }
        assert_eq!(size(&sizes, &next), expected, "after {taught} runs");
    }
}

/// Asserts that a pass records in each allocation which history sized its job: `cold` for a
/// job of a class with no runs that books nothing, and for one that names its CPU and leaves
/// its memory to the history; none for a job that names both. Catches a placement that
/// leaves `sized_from` unset (every start would then say `unknown`), and one that records a
/// history for a job the history did not size.
#[test]
fn a_pass_records_which_history_sized_each_placed_job() {
    let tables = Tables::new();
    let placer = farm_placer(FARM);
    for (id, class, (cpu, memory)) in [
        ("sized", "t-sized", (0, 0)),
        ("cpu-named", "t-cpu", (1000, 0)),
        ("both-named", "t-both", (1000, GIB)),
    ] {
        let new = js_jobs::NewJob {
            id: Some(JobId::parse(id).unwrap()),
            spec: of_class(class, needs(cpu, memory)),
            priority: 0,
            client: None,
        };
        tables.jobs.insert_queued(new).unwrap();
    }
    assert_eq!(pass(&placer, &tables).len(), 3);
    let sized_from = |id: &str| {
        let id = JobId::parse(id).unwrap();
        tables.jobs.get_allocation(&id).unwrap().unwrap().sized_from
    };
    assert_eq!(sized_from("sized"), Some(js_jobs::SizedFrom::Cold));
    assert_eq!(sized_from("cpu-named"), Some(js_jobs::SizedFrom::Cold));
    assert_eq!(sized_from("both-named"), None);
}

/// The cold size of the farm the Macs share: 4 cores and 16 GiB server-wide.
const SHARED: ColdSize = ColdSize {
    cpu_millis: 4000,
    memory_bytes: 16 * GIB,
};

/// A cold memory of 4 GiB for the jobs of `macos`.
fn macs_at_4_gib() -> ColdMemoryOn {
    ColdMemoryOn::new(BTreeMap::from([("macos".to_owned(), 4 * GIB)]))
}

/// A job of class `class`, with no runs, that books nothing and names `os` (none for `None`).
fn cold_on(id: &str, class: &str, os: Option<&str>) -> JobRecord {
    let mut spec = of_class(class, needs(0, 0));
    spec.constraints.os = os.map(str::to_owned);
    queued(id, spec)
}

/// Asserts that a class with no runs of a job whose `os` the server names books that operating
/// system's cold memory (4 GiB for `macos`), while a job of another `os` or of none books the
/// server's (16 GiB), each with the server's cold CPU; that an operating system's memory above
/// the largest host is held at that host's memory; and that a name is matched as the job gives
/// it (`macOS` is not `macos`). Catches a per-system memory that reaches the Linux jobs (the
/// farm's build hosts would then book a quarter of what they do), one that is not read, and one
/// that books more than any host has.
#[test]
fn a_job_of_an_os_the_server_names_books_that_os_s_cold_memory() {
    let mut sizes = JobSizes::with_cold(SHARED).with_cold_memory_on(macs_at_4_gib());
    sizes.set_bounds(farm_bounds());
    for (os, booked) in [
        (Some("macos"), (4000, 4 * GIB)),
        (Some("linux"), (4000, 16 * GIB)),
        (None, (4000, 16 * GIB)),
        (Some("macOS"), (4000, 16 * GIB)),
    ] {
        let job = cold_on("cold", "build", os);
        assert_eq!(size(&sizes, &job), booked, "{os:?}");
    }
    let huge = ColdMemoryOn::new(BTreeMap::from([("macos".to_owned(), 512 * GIB)]));
    let mut held = JobSizes::with_cold(SHARED).with_cold_memory_on(huge);
    held.set_bounds(farm_bounds());
    let job = cold_on("cold", "build", Some("macos"));
    assert_eq!(size(&held, &job), (4000, HOST.1));
    assert_eq!(ColdMemoryOn::default().of("macos"), None);
    assert_eq!(macs_at_4_gib().of("macos"), Some(4 * GIB));
}

/// A placer whose engine takes `cold` and, when given, `on`, over one Mac (28 cores and
/// 88 GiB of bookable memory, `os` `macos`), on a clock at zero.
fn mac_placer(cold: ColdSize, on: Option<ColdMemoryOn>) -> SchedPlacer {
    let clock = ManualClock::new(Timestamp::from_millis(0));
    let settings = Settings {
        cold,
        ..Settings::default()
    };
    let placer = SchedPlacer::new(settings, clock);
    let placer = match on {
        Some(on) => placer.with_cold_memory_on(on),
        None => placer,
    };
    let caps = BTreeMap::from([
        ("os".to_owned(), "macos".to_owned()),
        ("arch".to_owned(), "arm64".to_owned()),
    ]);
    let resources = Resources::new(28_000, 88 * GIB, 0, None).unwrap();
    placer
        .engine()
        .host_up(&host("mac"), &Shape::new(resources, caps), at(0));
    placer
}

/// Asserts what one pass places on one Mac of 40 queued `macos` jobs of 40 classes that have no
/// runs and book nothing, under a server-wide cold size of 4 cores and 16 GiB: without a cold
/// memory for `macos`, each books 16 GiB and five fill the Mac's memory (5 x 16 GiB fits 88 GiB,
/// 6 do not) whatever their CPU; with 4 GiB for `macos`, each books 4 GiB and seven are placed,
/// held by the CPU (7 x 4 cores fit the 28 cores at the CPU slack of 1.1, 8 do not). Catches a
/// placer whose engine is built without the operating system's memory.
#[test]
fn a_pass_on_a_mac_books_the_mac_s_cold_memory() {
    for (on, placed, each) in [
        (None, 5, booked(4000, 16 * GIB)),
        (Some(macs_at_4_gib()), 7, booked(4000, 4 * GIB)),
    ] {
        let tables = Tables::new();
        let placer = mac_placer(SHARED, on.clone());
        for n in 0..40 {
            let mut spec = of_class(&format!("target-{n:02}"), needs(0, 0));
            spec.constraints.os = Some("macos".to_owned());
            let new = js_jobs::NewJob {
                id: Some(JobId::parse(&format!("job-{n:02}")).unwrap()),
                spec,
                priority: 0,
                client: None,
            };
            tables.jobs.insert_queued(new).unwrap();
        }
        let ids = pass(&placer, &tables);
        assert_eq!(ids.len(), placed, "{on:?}");
        let first = JobId::parse("job-00").unwrap();
        let allocation = tables.jobs.get_allocation(&first).unwrap().unwrap();
        assert_eq!(allocation.booked, each, "{on:?}");
    }
}
