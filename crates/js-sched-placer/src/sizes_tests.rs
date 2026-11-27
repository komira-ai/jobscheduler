//! The sizes of the jobs that book nothing ([`JobSizes`]) and the history the placer teaches
//! them ([`SchedPlacer::learn_freed`] at each pass, [`SchedPlacer::learn_ended`] at once):
//! the job tables on the in-memory fake, every clock the test's.

use std::collections::BTreeMap;
use std::time::Duration;

use js_jobs::{
    AllocationStep, Booked, ClientName, CpuPeak, CpuThrottle, EndReason, JobEnd, JobId, JobRecord,
    JobSpec, JobState, Needs, NewJob, Output, ResourceUsage,
};
use js_registry::Placer;
use js_sizing::{Bounds, Day};
use js_state::Clock as _;
use js_types::sched::Resources as Capacity;
use js_types::{Labels, ManualClock, Resources, Shape, Timestamp};

use super::{
    ColdSize, JobSizes, SIZE_CLASS_LABEL, SizedFrom, bounds_of, class_name, day_of, micros, millis,
};
use crate::engine::Settings;
use crate::engine::jobs::{JobHosts, JobStarts, OpenHost, PassMetrics};
use crate::engine::rig::{Tables, at, host, spec_of};
use crate::place::SchedPlacer;

pub(super) const GIB: u64 = 1 << 30;
pub(super) const MIB: u64 = 1 << 20;

/// A host's bookable capacity: `cpu_millis` and `memory_bytes`.
fn capacity(cpu_millis: u64, memory_bytes: u64) -> Capacity {
    Capacity::new(cpu_millis, memory_bytes)
}

/// The cold size of this module's tests: two cores and 4 GiB, a quarter of [`bounds`]'s host
/// memory, so that a cold booking leaves the host room. The default cold size (22 cores and
/// 4 GiB) is the subject of `sizes_cold_tests` and of
/// [`a_job_that_books_nothing_is_booked_the_cold_size_until_its_class_has_runs`].
const MODULE_COLD: ColdSize = ColdSize {
    cpu_millis: 2000,
    memory_bytes: 4 * GIB,
};

/// The bounds of one host of 32 cores (a class books at most eight of them) and 16 GiB;
/// [`JobSizes::with_cold`] of [`MODULE_COLD`] puts its cold booking at two cores and 4 GiB.
fn bounds() -> Bounds {
    bounds_of([capacity(32_000, 16 * GIB)]).unwrap()
}

/// Needs of `cpu_millis` and `memory_bytes`.
pub(super) fn needs(cpu_millis: u64, memory_bytes: u64) -> Needs {
    Needs::Shape(Resources::new(cpu_millis, memory_bytes, 0, None).unwrap())
}

/// A share booked at `cpu_millis` and `memory_bytes`.
pub(super) fn booked(cpu_millis: u64, memory_bytes: u64) -> Booked {
    Booked::Shape {
        cpu_millis,
        memory_bytes,
        disk_bytes: 0,
        accelerators: 0,
    }
}

/// The spec of a job of the class `class` (by its label) that asks for `needs`.
pub(super) fn of_class(class: &str, needs: Needs) -> JobSpec {
    let mut spec = spec_of(needs, 600);
    let labels = BTreeMap::from([(SIZE_CLASS_LABEL.to_owned(), class.to_owned())]);
    spec.labels = Labels::from_caller(&labels).unwrap();
    spec
}

/// What the host of a new agent reported: `peak` bytes at most and `cpu` of CPU time, no CPU
/// pressure and no throttled period, and a CPU peak equal to the run's mean over the ten
/// seconds [`ran`] gives each run (the run's CPU is a measurement, and a class books the mean
/// its runs used, their CPU time over their wall time; the peak is reported, not booked).
pub(super) fn output(peak: u64, cpu: Duration) -> Output {
    Output {
        usage: Some(ResourceUsage {
            cpu_time: cpu,
            peak_memory_bytes: peak,
            cpu_throttle: Some(CpuThrottle {
                periods: 0,
                throttled_periods: 0,
            }),
            cpu_pressure_some: Some(Duration::ZERO),
            cpu_peak: Some(CpuPeak {
                millicores: u64::try_from(cpu.as_millis() / 10).unwrap(),
                window_usec: 5_000_000,
            }),
            peak_swap_bytes: None,
        }),
        ..Output::default()
    }
}

/// An exit with `code` after using `peak` bytes and `cpu` of CPU time.
pub(super) fn exited(code: i32, peak: u64, cpu: Duration) -> JobEnd {
    JobEnd::Exited {
        exit_code: code,
        output: output(peak, cpu),
    }
}

/// Records the job `id` of `spec` (for `client`, when one is given), places it booked as
/// `booked`, starts it at `start_ms` and ends it as `end` ten seconds later, freeing its
/// allocation: the ended record.
pub(super) fn ran(
    tables: &Tables,
    job: (&str, Option<&str>),
    spec: JobSpec,
    booked: Booked,
    end: JobEnd,
) -> JobRecord {
    ran_for(tables, job, spec, booked, end, Duration::from_secs(10))
}

/// [`ran`] with a wall time of `wall` instead of ten seconds.
pub(super) fn ran_for(
    tables: &Tables,
    (id, client): (&str, Option<&str>),
    spec: JobSpec,
    booked: Booked,
    end: JobEnd,
    wall: Duration,
) -> JobRecord {
    let id = JobId::parse(id).unwrap();
    let new = NewJob {
        id: Some(id.clone()),
        spec,
        priority: 0,
        client: client.map(|client| ClientName::parse(client).unwrap()),
    };
    tables.jobs.insert_queued(new).unwrap();
    tables.jobs.place(&id, host("a"), booked).unwrap();
    tables
        .jobs
        .transition(&id, JobState::Placed, JobState::Starting)
        .unwrap();
    tables
        .jobs
        .transition(&id, JobState::Starting, JobState::Running)
        .unwrap();
    let start = tables.clock.now().as_duration().as_millis();
    tables.set_ms(u64::try_from(start).unwrap() + u64::try_from(wall.as_millis()).unwrap());
    let (job, _) = tables
        .jobs
        .end_and_free(&id, end, EndReason::Exited)
        .unwrap();
    job
}

/// `n` runs of `class` booked as `booked`, each ending as `end` gives: the ended records, for
/// the ids `<prefix>-<n>`.
pub(super) fn runs(
    tables: &Tables,
    prefix: &str,
    n: usize,
    class: &str,
    booked: Booked,
    end: impl Fn() -> JobEnd,
) -> Vec<JobRecord> {
    (0..n)
        .map(|at| {
            let id = format!("{prefix}-{at}");
            ran(
                tables,
                (&id, None),
                of_class(class, needs(0, 0)),
                booked,
                end(),
            )
        })
        .collect()
}

/// Sizes taught `jobs` (each booked as `booked`) under [`bounds`].
fn taught(jobs: &[JobRecord], booked: Booked) -> JobSizes {
    let mut sizes = JobSizes::with_cold(MODULE_COLD);
    sizes.set_bounds(Some(bounds()));
    for job in jobs {
        assert!(sizes.observe(job, booked), "{} teaches a run", job.id);
    }
    sizes
}

/// The CPU and memory `sizes` books for the queued job `job` on day 0.
fn size(sizes: &JobSizes, job: &JobRecord) -> (u64, u64) {
    match &sizes.sized(job, Day(0)).spec.needs {
        Needs::Shape(resources) => (resources.cpu_millis(), resources.memory_bytes()),
        Needs::WholeHost => panic!("a share is sized as a share"),
    }
}

/// A queued job of `spec`, recorded in fresh tables as `id`.
fn queued(id: &str, spec: JobSpec) -> JobRecord {
    queued_by(None, id, spec)
}

/// A queued job of `spec` submitted by `client`, recorded in fresh tables as `id`.
fn queued_by(client: Option<&str>, id: &str, spec: JobSpec) -> JobRecord {
    let tables = Tables::new();
    let new = NewJob {
        id: Some(JobId::parse(id).unwrap()),
        spec,
        priority: 0,
        client: client.map(|client| ClientName::parse(client).unwrap()),
    };
    tables.jobs.insert_queued(new).unwrap().record
}

/// Asserts that a job that books nothing is booked the default cold size (22 cores and 4 GiB,
/// held at a quarter of the largest host's cores and at its bookable memory) while neither its class nor its client
/// has runs, that a job that names both its CPU and its memory, and one that books the whole
/// host, are booked as they ask, and that before any host is known every job is left as it is.
/// Catches a cold size held at the smallest host (half a core and 2 GiB here), a sizing that
/// overrides a declared booking, and a whole-host job made a share.
#[test]
fn a_job_that_books_nothing_is_booked_the_cold_size_until_its_class_has_runs() {
    let mut sizes = JobSizes::new();
    let nothing = queued("nothing", of_class("build", needs(0, 0)));
    assert_eq!(
        sizes.sized(&nothing, Day(0)),
        nothing,
        "no host known: as it is"
    );
    sizes.set_bounds(bounds_of([
        capacity(2000, 2 * GIB),
        capacity(16_000, 16 * GIB),
    ]));
    assert_eq!(size(&sizes, &nothing), (4000, 4 * GIB));
    let declared = queued("declared", of_class("build", needs(500, GIB)));
    assert_eq!(sizes.sized(&declared, Day(0)), declared);
    let whole = queued("whole", of_class("build", Needs::WholeHost));
    assert_eq!(sizes.sized(&whole, Day(0)), whole);
}

/// Asserts that a job that names only its CPU keeps it and has its memory sized, and one that
/// names only its memory keeps it and has its CPU sized, every other dimension kept. Catches a
/// sizing that replaces a named dimension, or drops the disk or the GPUs.
#[test]
fn a_dimension_the_job_names_is_kept_and_only_the_other_is_sized() {
    let mut sizes = JobSizes::with_cold(MODULE_COLD);
    sizes.set_bounds(Some(bounds()));
    let cpu_only = queued("cpu-only", of_class("build", needs(500, 0)));
    assert_eq!(size(&sizes, &cpu_only), (500, 4 * GIB));
    let disk = Resources::new(0, GIB, 0, None).unwrap().with_disk_bytes(7);
    let memory_only = queued("memory-only", of_class("build", Needs::Shape(disk)));
    let Needs::Shape(sized) = sizes.sized(&memory_only, Day(0)).spec.needs else {
        panic!("a share is sized as a share");
    };
    assert_eq!((sized.cpu_millis(), sized.memory_bytes()), (2000, GIB));
    assert_eq!((sized.disk_bytes(), sized.gpus()), (7, 0));
}

/// Asserts that the successful runs of a class size its next job from them, from even the
/// first: the memory is `1.2 x` their peak plus 128 MiB rounded up to 256 MiB (2 GiB peaks give
/// 2.75 GiB, whether four runs or five) and the CPU what they used per second (30 s of CPU in
/// 10 s is three cores, under the quarter of the host's cores a class may book). Another class
/// of the same client books from that client's history (the same figures); a class of another
/// client, which has none, is still cold. Catches a young class held at the cold booking, a CPU
/// not learned, and a history shared across clients.
#[test]
fn runs_size_the_class_from_their_peak_and_cpu_and_a_new_class_from_its_clients() {
    let tables = Tables::new();
    let end = || exited(0, 2 * GIB, Duration::from_secs(30));
    let jobs = runs(&tables, "build", 5, "build", booked(2000, 4 * GIB), end);
    let next = queued("next", of_class("build", needs(0, 0)));
    let four = taught(&jobs[..4], booked(2000, 4 * GIB));
    assert_eq!(size(&four, &next), (3000, 11 * 256 * MIB));
    let five = taught(&jobs, booked(2000, 4 * GIB));
    assert_eq!(size(&five, &next), (3000, 11 * 256 * MIB));
    let other = queued("other", of_class("test", needs(0, 0)));
    assert_eq!(size(&five, &other), (3000, 11 * 256 * MIB), "the client's");
    assert_eq!(five.sized_from(&other, Day(0)), Some(SizedFrom::Fallback));
    let stranger = queued_by(Some("stranger"), "stranger", of_class("test", needs(0, 0)));
    assert_eq!(
        size(&five, &stranger),
        (2000, 4 * GIB),
        "another client: cold"
    );
    assert_eq!(five.sized_from(&stranger, Day(0)), Some(SizedFrom::Cold));
}

/// Asserts that runs of one image and command that name no class share a history although
/// each names its own action digest, lease and deadline in its arguments (as each job of a
/// build farm does): after five runs of 2 GiB peak and three cores, the next run with other
/// arguments books what they used, not the cold size. Catches a class that holds the
/// arguments, which makes every run its own class and books every one of them cold.
#[test]
fn runs_that_differ_only_in_their_arguments_share_one_history() {
    let tables = Tables::new();
    let task = |at: usize| {
        let mut spec = spec_of(needs(0, 0), 600);
        spec.command = Vec::new();
        spec.args = [
            "--action",
            &format!("{at:064x}/1234"),
            "--lease",
            &format!("lease-{at}"),
            "--deadline-ms",
            &(600_000 + at).to_string(),
        ]
        .map(str::to_owned)
        .to_vec();
        spec
    };
    let jobs: Vec<JobRecord> = (0..5)
        .map(|at| {
            ran(
                &tables,
                (&format!("action-{at}"), None),
                task(at),
                booked(2000, 4 * GIB),
                exited(0, 2 * GIB, Duration::from_secs(30)),
            )
        })
        .collect();
    let sizes = taught(&jobs, booked(2000, 4 * GIB));
    let next = queued("action-next", task(99));
    assert_eq!(size(&sizes, &next), (3000, 11 * 256 * MIB));
}

/// Asserts that the class is the label's value when the job has one, whatever its image's
/// digest, command or arguments (each run of a build names a new commit), and its image's
/// name and command otherwise, without the image's digest or the arguments; and that two
/// clients, or two platforms, never share a class. Catches a key that holds the commit, the
/// digest or the arguments, or that pools tenants or platforms.
#[test]
fn the_class_is_the_label_else_the_command_under_the_client_and_platform() {
    let tables = Tables::new();
    let label = |id: &str, args: &[&str]| {
        let mut spec = of_class("build", needs(0, 0));
        spec.args = args.iter().map(|arg| (*arg).to_owned()).collect();
        queued(id, spec)
    };
    let one = label("one", &["abc123"]);
    assert_eq!(
        JobSizes::key_of(&one),
        JobSizes::key_of(&label("two", &["def456"]))
    );
    let run = |id: &str, digest: char, command: &str, args: &[&str]| {
        let mut spec = spec_of(needs(0, 0), 600);
        spec.command = vec![command.to_owned()];
        spec.image = js_types::ImageDigestRef::parse(&format!(
            "team/app@sha256:{}",
            digest.to_string().repeat(64)
        ))
        .ok();
        spec.args = args.iter().map(|arg| (*arg).to_owned()).collect();
        queued(id, spec)
    };
    let plain = JobSizes::key_of(&run("a", 'a', "run", &["x"]));
    assert_eq!(
        plain,
        JobSizes::key_of(&run("b", 'b', "run", &["x"])),
        "the digest is not the class"
    );
    assert_eq!(
        plain,
        JobSizes::key_of(&run("c", 'a', "run", &["y"])),
        "nor are the arguments"
    );
    assert_ne!(
        plain,
        JobSizes::key_of(&run("d", 'a', "bench", &["x"])),
        "the command is"
    );
    let mut renamed = spec_of(needs(0, 0), 600);
    renamed.image =
        js_types::ImageDigestRef::parse(&format!("team/other@sha256:{}", "a".repeat(64))).ok();
    renamed.args = vec!["x".to_owned()];
    assert_ne!(
        plain,
        JobSizes::key_of(&queued("renamed", renamed)),
        "the image's name is"
    );
    assert_ne!(plain, JobSizes::key_of(&one), "a label is not a command");
    let mut spec = spec_of(needs(0, 0), 600);
    spec.image = None;
    assert_ne!(plain, JobSizes::key_of(&queued("bare", spec)));
    let alice = ran(
        &tables,
        ("alice-run", Some("alice")),
        of_class("build", needs(0, 0)),
        booked(1, 1),
        exited(0, 1, Duration::ZERO),
    );
    assert_ne!(
        JobSizes::key_of(&alice),
        JobSizes::key_of(&one),
        "a client is its own tenant"
    );
    let mut arm = of_class("build", needs(0, 0));
    arm.constraints.os = Some("linux".to_owned());
    arm.constraints.arch = Some("aarch64".to_owned());
    assert_ne!(
        JobSizes::key_of(&queued("arm", arm)),
        JobSizes::key_of(&one)
    );
}

/// Asserts that a run stopped for memory raises its class above the booking it died at: the
/// next job of the class books twice that booking, where a cold job of it booked 4 GiB.
/// Catches a kill read as a low sample (the class would stay at the size that killed it).
#[test]
fn a_memory_kill_raises_the_class_to_twice_the_booking_it_died_at() {
    let tables = Tables::new();
    let killed = JobEnd::Memory {
        exit_code: None,
        output: output(4 * GIB, Duration::from_secs(5)),
    };
    let job = ran(
        &tables,
        ("killed", None),
        of_class("build", needs(0, 0)),
        booked(2000, 4 * GIB),
        killed,
    );
    let sizes = taught(&[job], booked(2000, 4 * GIB));
    let next = queued("next", of_class("build", needs(0, 0)));
    assert_eq!(size(&sizes, &next), (2000, 8 * GIB));
}

/// Asserts that a class whose runs would need the host's whole bookable memory books all of
/// it (and the CPU its runs used, a core), so it waits for an empty host rather than being
/// cut. Catches an over-large class booked cold, where it would be killed.
#[test]
fn a_class_whose_runs_reach_the_host_books_all_of_its_memory() {
    let tables = Tables::new();
    let end = || exited(0, 14 * GIB, Duration::from_secs(10));
    let jobs = runs(&tables, "huge", 2, "huge", booked(2000, 16 * GIB), end);
    let sizes = taught(&jobs, booked(2000, 16 * GIB));
    let next = queued("next", of_class("huge", needs(0, 0)));
    assert_eq!(size(&sizes, &next), (1000, 16 * GIB));
}

/// Asserts that runs that failed, were stopped at their wall limit, failed an input or were
/// cancelled teach the memory nothing: five of each with a small peak leave the class at the
/// cold booking. Catches a failed build's early, low peak lowering its class.
#[test]
fn failed_or_cancelled_runs_teach_the_memory_nothing() {
    let tables = Tables::new();
    let output = || output(GIB / 4, Duration::from_secs(1));
    let end_of = |prefix: &str| match prefix {
        "failed" => exited(1, GIB / 4, Duration::from_secs(1)),
        "wall" => JobEnd::WallLimit {
            exit_code: None,
            output: output(),
        },
        "cancelled" => JobEnd::Cancelled {
            exit_code: None,
            output: output(),
        },
        _ => JobEnd::InputFailed {
            exit_code: None,
            input_failure: "input-unreadable".to_owned(),
            output: output(),
        },
    };
    let next = queued("next", of_class("build", needs(0, 0)));
    for prefix in ["failed", "wall", "cancelled", "input"] {
        let end = || end_of(prefix);
        let jobs = runs(&tables, prefix, 5, "build", booked(2000, 4 * GIB), end);
        let mut sizes = JobSizes::with_cold(MODULE_COLD);
        sizes.set_bounds(Some(bounds()));
        for job in &jobs {
            let taught = sizes.observe(job, booked(2000, 4 * GIB));
            assert!(!taught, "{} is seen and teaches nothing", job.id);
        }
        assert_eq!(size(&sizes, &next).1, 4 * GIB, "{prefix}");
    }
}

/// Asserts that a job that never ran, booked the whole host or was lost with no result
/// teaches nothing. Catches a run built from a job with no start, no result or no share.
#[test]
fn a_job_that_never_ran_had_the_whole_host_or_no_result_teaches_nothing() {
    let tables = Tables::new();
    let mut sizes = JobSizes::with_cold(MODULE_COLD);
    let ok = ran(
        &tables,
        ("ok", None),
        of_class("build", needs(0, 0)),
        booked(1, GIB),
        exited(0, GIB, Duration::ZERO),
    );
    assert!(!sizes.observe(&ok, Booked::WholeHost), "the whole host");
    let lost = ran(
        &tables,
        ("lost", None),
        of_class("build", needs(0, 0)),
        booked(1, GIB),
        JobEnd::Lost,
    );
    assert!(!sizes.observe(&lost, booked(1, GIB)), "no result");
    let id = JobId::parse("unstarted").unwrap();
    let new = NewJob {
        id: Some(id.clone()),
        spec: of_class("build", needs(0, 0)),
        priority: 0,
        client: None,
    };
    tables.jobs.insert_queued(new).unwrap();
    tables.jobs.place(&id, host("a"), booked(1, GIB)).unwrap();
    let cancelled = JobEnd::Cancelled {
        exit_code: None,
        output: Output::default(),
    };
    let (unstarted, _) = tables
        .jobs
        .end_and_free(&id, cancelled, EndReason::Cancelled)
        .unwrap();
    assert!(!sizes.observe(&unstarted, booked(1, GIB)), "never started");
}

/// Asserts that a success's peak sizes its class at least from its peak whatever its booking,
/// since a booking is no limit (a contained job has no `memory.max`): five runs at 4 GiB less
/// 100 MiB under a 4 GiB booking book the next job 5 GiB (they reached their booking, a step
/// above it, which is also their peak with the margin), one run that used 6 GiB under a 4 GiB
/// booking raises the next booking to 7.5 GiB at once and nothing kills it (it is a success,
/// which teaches no floor), and runs at 90% of the booking book theirs. Catches a run at or
/// above its booking taken as held at it with no margin, which keeps the class at a booking its
/// jobs outgrow.
#[test]
fn a_success_at_or_above_its_booking_books_its_peak_and_raises_the_next_at_once() {
    let tables = Tables::new();
    let near = || exited(0, 4 * GIB - 100 * MIB, Duration::from_secs(10));
    let jobs = runs(&tables, "near", 5, "build", booked(2000, 4 * GIB), near);
    let next = queued("next", of_class("build", needs(0, 0)));
    assert_eq!(
        size(&taught(&jobs, booked(2000, 4 * GIB)), &next).1,
        5 * GIB
    );
    let over = || exited(0, 6 * GIB, Duration::from_secs(10));
    let mut jobs = jobs;
    jobs.extend(runs(
        &tables,
        "over",
        1,
        "build",
        booked(2000, 4 * GIB),
        over,
    ));
    let sizes = taught(&jobs, booked(2000, 4 * GIB));
    assert_eq!(size(&sizes, &next).1, 30 * 256 * MIB, "raised at once");
    let class = sizes.summaries.get(&JobSizes::key_of(&next)).unwrap();
    assert_eq!(class.oom_floor(), 0, "a success raises no floor");
    let below = || exited(0, 4 * GIB - 400 * MIB, Duration::from_secs(10));
    let jobs = runs(&tables, "below", 5, "build", booked(2000, 4 * GIB), below);
    assert_eq!(
        size(&taught(&jobs, booked(2000, 4 * GIB)), &next).1,
        18 * 256 * MIB
    );
    let unbooked = runs(&tables, "zero", 1, "build", booked(2000, 0), near);
    let mut sizes = JobSizes::with_cold(MODULE_COLD);
    sizes.set_bounds(Some(bounds()));
    assert!(
        sizes.observe(&unbooked[0], booked(2000, 0)),
        "a run with no booking is a run"
    );
}

/// Asserts that runs taught before any host is known are kept and taught when the first
/// bounds come, and that bounds of no host keep the last ones. Catches the runs a server
/// replays at its start being taught against bounds no host has, or lost.
#[test]
fn runs_taught_before_any_host_is_known_are_taught_when_one_is() {
    let tables = Tables::new();
    let end = || exited(0, 2 * GIB, Duration::from_secs(30));
    let jobs = runs(&tables, "build", 5, "build", booked(2000, 4 * GIB), end);
    let mut sizes = JobSizes::with_cold(MODULE_COLD);
    for job in &jobs {
        assert!(sizes.observe(job, booked(2000, 4 * GIB)));
    }
    let next = queued("next", of_class("build", needs(0, 0)));
    sizes.set_bounds(None);
    assert_eq!(sizes.sized(&next, Day(0)), next);
    sizes.set_bounds(Some(bounds()));
    assert_eq!(size(&sizes, &next), (3000, 11 * 256 * MIB));
    sizes.set_bounds(None);
    assert_eq!(
        size(&sizes, &next),
        (3000, 11 * 256 * MIB),
        "the last bounds stay"
    );
}

/// Asserts that each allocation gone from the live set since the last call is named once,
/// with what it booked. Catches a run taught twice, or a free never seen.
#[test]
fn freed_names_each_allocation_gone_since_the_last_call_once() {
    let tables = Tables::new();
    let mut ids = Vec::new();
    for id in ["first", "second"] {
        let id = JobId::parse(id).unwrap();
        let new = NewJob {
            id: Some(id.clone()),
            spec: spec_of(needs(1, 1), 600),
            priority: 0,
            client: None,
        };
        tables.jobs.insert_queued(new).unwrap();
        ids.push(tables.jobs.place(&id, host("a"), booked(1, 2)).unwrap().1);
    }
    let mut sizes = JobSizes::with_cold(MODULE_COLD);
    assert_eq!(sizes.freed(&ids), Vec::new());
    assert_eq!(
        sizes.freed(&ids[1..]),
        vec![(ids[0].job.clone(), booked(1, 2))]
    );
    assert_eq!(sizes.freed(&ids[1..]), Vec::new());
}

/// Asserts the bounds of a set of hosts: none with no host or no memory; otherwise the
/// largest host's memory and cores, with a cold booking of the default 4 GiB held at that
/// memory (4 GiB on a host of 16 GiB and 100 bytes or of 251 GiB, 2 GiB on a host of 2 GiB),
/// and cores beyond what the estimator counts held at its most. Catches a cold booking taken
/// from the host's size, one above the host, or a count that wraps.
#[test]
fn the_bounds_are_the_largest_hosts_with_the_default_cold_memory() {
    assert_eq!(bounds_of([]), None);
    assert_eq!(bounds_of([capacity(8000, 0)]), None);
    let odd = bounds_of([capacity(8000, 16 * GIB + 100), capacity(1000, GIB)]).unwrap();
    assert_eq!(
        (odd.default_memory, odd.host_memory, odd.node_cpu_millis),
        (4 * GIB, 16 * GIB + 100, 8000)
    );
    assert_eq!(
        bounds_of([capacity(88_000, 251 * GIB)])
            .unwrap()
            .default_memory,
        4 * GIB
    );
    assert_eq!(
        bounds_of([capacity(1000, 2 * GIB)]).unwrap().default_memory,
        2 * GIB
    );
    let many = bounds_of([capacity(u64::MAX, GIB)]).unwrap();
    assert_eq!(many.node_cpu_millis, u32::MAX);
}

/// Asserts that the estimator's day is the whole days since the epoch, held at its most past
/// what a day counts, and that durations convert saturating. Catches a day off by one at
/// midnight or a conversion that wraps.
#[test]
fn days_and_durations_convert_whole_and_saturating() {
    assert_eq!(day_of(at(0)), Day(0));
    assert_eq!(day_of(at(86_399_999)), Day(0));
    assert_eq!(day_of(at(86_400_000)), Day(1));
    assert_eq!(day_of(at(u64::MAX)), Day(u32::MAX));
    assert_eq!(
        (millis(Duration::MAX), micros(Duration::MAX)),
        (u64::MAX, u64::MAX)
    );
    assert_eq!(
        (
            millis(Duration::from_micros(1500)),
            micros(Duration::from_micros(1500))
        ),
        (1, 1500)
    );
}

/// What a pass told: nothing the tests read.
struct Quiet;

impl PassMetrics for Quiet {
    fn job_placed(&self, _: Duration) {}
    fn store_write_failed(&self) {}
}

impl JobStarts for Quiet {
    fn placed(&self, _: &JobId, _: &js_types::HostId) {}
    fn tainted(&self, _: &js_types::HostId, _: &JobId) {}
}

/// A placer over the host `a` of eight cores and 16 GiB, on a clock at zero.
pub(super) fn placer() -> SchedPlacer {
    let clock = ManualClock::new(Timestamp::from_millis(0));
    let settings = Settings {
        cold: MODULE_COLD,
        ..Settings::default()
    };
    let placer = SchedPlacer::new(settings, clock);
    let caps = BTreeMap::from([("arch".to_owned(), "x86_64".to_owned())]);
    let shape = Shape::new(Resources::new(32_000, 16 * GIB, 0, None).unwrap(), caps);
    placer.engine().host_up(&host("a"), &shape, at(0));
    placer
}

/// One pass of `placer` over `tables` with `a` open: the jobs it placed.
pub(super) fn pass(placer: &SchedPlacer, tables: &Tables) -> Vec<JobId> {
    placer.heard(&host("a"), None);
    let open = BTreeMap::from([(host("a"), OpenHost::default())]);
    let hello = std::collections::BTreeSet::new();
    let hosts = JobHosts {
        open: &open,
        hello: &hello,
        tainted: &std::collections::BTreeSet::new(),
        whole: None,
    };
    placer
        .place_jobs_with(&tables.jobs, &hosts, (&Quiet, &Quiet))
        .unwrap()
}

/// What the allocation of `id` books.
pub(super) fn allocation(tables: &Tables, id: &str) -> Booked {
    let id = JobId::parse(id).unwrap();
    tables.jobs.get_allocation(&id).unwrap().unwrap().booked
}

/// Records the queued job `id` of `class` that books nothing.
pub(super) fn submit(tables: &Tables, id: &str, class: &str) {
    submit_by(tables, id, class, None);
}

/// Records the queued job `id` of `class` that books nothing, submitted by `client`.
fn submit_by(tables: &Tables, id: &str, class: &str, client: Option<&str>) {
    let new = NewJob {
        id: Some(JobId::parse(id).unwrap()),
        spec: of_class(class, needs(0, 0)),
        priority: 0,
        client: client.map(|client| ClientName::parse(client).unwrap()),
    };
    tables.jobs.insert_queued(new).unwrap();
}

/// Starts the placed job `id` and ends it with a 2 GiB peak and three cores, freeing it.
fn finish(tables: &Tables, id: &str) {
    finish_at(tables, id, 2 * GIB);
}

/// Starts the placed job `id` and ends it with a peak of `peak` bytes and three cores, freeing
/// it.
pub(super) fn finish_at(tables: &Tables, id: &str, peak: u64) {
    let id = JobId::parse(id).unwrap();
    tables
        .jobs
        .transition(&id, JobState::Placed, JobState::Starting)
        .unwrap();
    tables
        .jobs
        .transition(&id, JobState::Starting, JobState::Running)
        .unwrap();
    let start = tables.clock.now().as_duration().as_millis();
    tables.set_ms(u64::try_from(start).unwrap() + 10_000);
    let end = exited(0, peak, Duration::from_secs(30));
    tables
        .jobs
        .end_and_free(&id, end, EndReason::Exited)
        .unwrap();
}

/// Asserts the pass end to end: a job that books nothing is placed booked the cold size (its
/// own needs stay as submitted), each run is learned once its allocation is freed (from the
/// first run on, the class books what its runs used: their CPU and `1.2 x` their peak plus
/// 128 MiB), and after five runs the next job of the class is booked the same.
/// Catches a pass that books 0 for a job that names nothing (which packs a host without
/// bound), a size written over the job's own needs (a retried submit would no longer be the
/// same job), and a job placed and freed between two passes never learned.
#[test]
fn a_pass_books_a_job_that_books_nothing_from_its_class_and_learns_each_freed_run() {
    let tables = Tables::new();
    let placer = placer();
    for run in 0..5 {
        let id = format!("build-{run}");
        submit(&tables, &id, "build");
        assert_eq!(pass(&placer, &tables), vec![JobId::parse(&id).unwrap()]);
        let expected = if run == 0 {
            booked(2000, 4 * GIB)
        } else {
            booked(3000, 11 * 256 * MIB)
        };
        assert_eq!(
            allocation(&tables, &id),
            expected,
            "run {run}: cold until a run is learned, then what the runs used"
        );
        let record = tables
            .jobs
            .get_job(&JobId::parse(&id).unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(record.spec.needs, needs(0, 0), "the job's own needs stay");
        finish(&tables, &id);
    }
    submit(&tables, "next", "build");
    pass(&placer, &tables);
    assert_eq!(allocation(&tables, "next"), booked(3000, 11 * 256 * MIB));
}

/// Asserts that a freed job the store no longer holds, or whose read fails, teaches nothing
/// and ends nothing: the class stays cold. Catches a failed read that stops the pass.
#[test]
fn a_freed_job_that_cannot_be_read_teaches_nothing() {
    let tables = Tables::new();
    let other = Tables::new();
    let placer = placer();
    submit(&other, "elsewhere", "build");
    let id = JobId::parse("elsewhere").unwrap();
    let (_, row) = other.jobs.place(&id, host("a"), booked(1, 1)).unwrap();
    placer.learn_freed(&tables.jobs, std::slice::from_ref(&row));
    placer.learn_freed(&tables.jobs, &[]);
    submit(&tables, "held", "build");
    let held = JobId::parse("held").unwrap();
    let (_, row) = tables.jobs.place(&held, host("a"), booked(1, 1)).unwrap();
    placer.learn_freed(&tables.jobs, std::slice::from_ref(&row));
    tables.store.fail_read_after(0);
    placer.learn_freed(&tables.jobs, &[]);
    let next = queued("next", of_class("build", needs(0, 0)));
    let mut engine = placer.engine();
    engine.sizes().set_bounds(Some(bounds()));
    assert_eq!(size(engine.sizes(), &next), (2000, 4 * GIB));
}

/// Asserts what a server's start teaches: every ended job the store holds whose allocation is
/// freed, over more than one page of the table, with what it booked, and nothing of a queued, a
/// live or an ended but still releasing job; those are learned when a later pass sees them
/// freed. A store that fails the read
/// answers its error. Catches a restart that forgets the history, a replay that stops at the
/// first page, and a live job taught twice or never.
#[test]
fn a_start_teaches_every_ended_job_and_a_later_pass_each_job_live_then() {
    let new_id = |filler: usize| JobId::parse(&format!("zz-{filler:04}")).unwrap();
    let tables = Tables::new();
    let end = || exited(0, 2 * GIB, Duration::from_secs(30));
    runs(&tables, "build", 4, "build", booked(2000, 4 * GIB), end);
    for filler in 0..1000 {
        let new = NewJob {
            id: Some(new_id(filler)),
            spec: spec_of(needs(1, 1), 600),
            priority: 0,
            client: None,
        };
        tables.jobs.insert_queued(new).unwrap();
        let id = new_id(filler);
        tables.jobs.place(&id, host("a"), booked(1, 1)).unwrap();
        let cancelled = JobEnd::Cancelled {
            exit_code: None,
            output: Output::default(),
        };
        let reason = EndReason::Cancelled;
        tables.jobs.end_and_free(&id, cancelled, reason).unwrap();
    }
    submit(&tables, "live", "build");
    let live = JobId::parse("live").unwrap();
    tables
        .jobs
        .place(&live, host("a"), booked(2000, 4 * GIB))
        .unwrap();
    submit(&tables, "releasing", "build");
    let releasing = JobId::parse("releasing").unwrap();
    tables
        .jobs
        .place(&releasing, host("a"), booked(2000, 4 * GIB))
        .unwrap();
    for (from, to) in [
        (JobState::Placed, JobState::Starting),
        (JobState::Starting, JobState::Running),
    ] {
        tables.jobs.transition(&releasing, from, to).unwrap();
    }
    let started = tables.clock.now().as_duration().as_millis();
    tables.set_ms(u64::try_from(started).unwrap() + 10_000);
    let end = exited(0, 2 * GIB, Duration::from_secs(30));
    tables.jobs.end_job(&releasing, end).unwrap();
    tables.jobs.mark_releasing(&releasing).unwrap();
    let placer = placer();
    assert_eq!(
        placer.learn_ended(&tables.jobs),
        Ok(4),
        "a job ended but not yet freed waits for its free"
    );
    submit(&tables, "next", "build");
    pass(&placer, &tables);
    assert_eq!(
        allocation(&tables, "next"),
        booked(3000, 11 * 256 * MIB),
        "four runs: what they used"
    );
    finish_at(&tables, "live", 3 * GIB);
    let freed = tables.jobs.free(&releasing, EndReason::Exited).unwrap();
    assert_eq!(freed.step, AllocationStep::Freed);
    submit(&tables, "after", "build");
    pass(&placer, &tables);
    assert_eq!(
        allocation(&tables, "after"),
        booked(3000, 15 * 256 * MIB),
        "six with the live one (3 GiB) and the releasing one"
    );
    assert_eq!(
        placer.learn_ended(&tables.jobs),
        Ok(6),
        "each ended run once"
    );
    tables.store.fail_read_after(0);
    assert!(placer.learn_ended(&tables.jobs).is_err());
}

/// Asserts the name a class is recorded under: the `size-class` label as it is, else `run-` and
/// 16 hex digits that are the same for one command, whatever its arguments, and differ for
/// another command. Catches a name built from the job's id or its arguments, which a repeat run
/// never shares.
#[test]
fn a_class_is_named_by_its_label_else_by_the_hash_of_its_command() {
    let labelled = queued("one", of_class("kbf.cargo-test.kbf-server", needs(0, 0)));
    assert_eq!(class_name(&labelled), "kbf.cargo-test.kbf-server");
    let plain = |id: &str, command: &str, args: &[&str]| {
        let mut spec = spec_of(needs(0, 0), 600);
        spec.command = vec![command.to_owned()];
        spec.args = args.iter().map(|arg| (*arg).to_owned()).collect();
        queued(id, spec)
    };
    let name = class_name(&plain("first", "test", &["--lease", "1"]));
    let digits = name.strip_prefix("run-").unwrap();
    assert_eq!(digits.len(), 16, "{name}");
    assert!(digits.bytes().all(|b| b.is_ascii_hexdigit()), "{name}");
    assert_eq!(
        class_name(&plain("second", "test", &["--lease", "2"])),
        name,
        "a repeat run"
    );
    assert_ne!(
        class_name(&plain("third", "bench", &["--lease", "1"])),
        name,
        "another command"
    );
}

#[path = "sizes_cold_tests.rs"]
mod cold;

#[path = "sizes_fallback_tests.rs"]
mod fallback;

#[path = "sizes_named_tests.rs"]
mod named;

#[path = "sizes_client_tests.rs"]
mod client;

#[path = "sizes_farm_tests.rs"]
mod farm;

#[path = "sizes_follows_use_tests.rs"]
mod follows_use;

#[path = "sizes_memory_series_tests.rs"]
mod memory_series;

#[path = "sizes_burst_tests.rs"]
mod burst;

#[path = "sizes_at_booking_tests.rs"]
mod at_booking;

#[path = "sizes_signal_tests.rs"]
mod signal;

#[path = "sizes_swap_tests.rs"]
mod swap;
