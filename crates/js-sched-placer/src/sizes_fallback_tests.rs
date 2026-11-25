//! The fallback class ([`SIZE_FALLBACK_LABEL`]): a job whose own class has too few runs is
//! booked from the history of the class it names there (a build step's kind, where its
//! `size-class` is its target), else from its client's, before the cold size, and every run
//! teaches both classes and the client's. The host is the module's: 32 cores and 16 GiB,
//! where the module's cold size is two cores and 4 GiB.

use std::collections::BTreeMap;
use std::io;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use js_jobs::{ClientName, JobEnd, JobId, JobRecord, JobSpec, Needs, NewJob};
use js_sizing::Day;
use js_types::{Labels, ManualClock, Resources, Shape, Timestamp};

use super::super::{SIZE_FALLBACK_LABEL, SizedFrom};
use super::{
    GIB, JobSizes, MIB, MODULE_COLD, SIZE_CLASS_LABEL, Tables, allocation, booked, bounds, exited,
    host, needs, of_class, output, pass, queued, queued_by, ran, size, spec_of, taught,
};
use crate::engine::Settings;
use crate::place::SchedPlacer;

/// The kind every target of these tests is a step of.
const KIND: &str = "zig-cc";

/// The milliseconds of a day.
const DAY_MS: u64 = 86_400_000;

/// The spec of a job of the class `class` whose fallback class is `fallback`, asking `needs`.
fn of_kind(class: &str, fallback: &str, needs: js_jobs::Needs) -> JobSpec {
    let mut spec = spec_of(needs, 600);
    let labels = BTreeMap::from([
        (SIZE_CLASS_LABEL.to_owned(), class.to_owned()),
        (SIZE_FALLBACK_LABEL.to_owned(), fallback.to_owned()),
    ]);
    spec.labels = Labels::from_caller(&labels).unwrap();
    spec
}

/// A queued job `id` of the target `class`, a step of [`KIND`], that books nothing.
fn step(id: &str, class: &str) -> JobRecord {
    queued(id, of_kind(class, KIND, needs(0, 0)))
}

/// One run, ten seconds long, of each of `targets` (each its own class, a step of `kind`),
/// booked two cores and 4 GiB, peaking at `peak` bytes and using `cpu` of CPU time.
fn kind_runs(
    tables: &Tables,
    targets: &[&str],
    kind: &str,
    (peak, cpu): (u64, Duration),
) -> Vec<JobRecord> {
    targets
        .iter()
        .map(|target| {
            let spec = of_kind(target, kind, needs(0, 0));
            ran(
                tables,
                (&format!("{target}-run"), None),
                spec,
                booked(2000, 4 * GIB),
                exited(0, peak, cpu),
            )
        })
        .collect()
}

/// Five targets, one run each, that teach [`KIND`] a peak of `peak` bytes and `cpu` of CPU
/// time in ten seconds.
fn five_targets(tables: &Tables, peak: u64, cpu: Duration) -> Vec<JobRecord> {
    let targets = ["t0", "t1", "t2", "t3", "t4"];
    kind_runs(tables, &targets, KIND, (peak, cpu))
}

/// Asserts that a target with no runs of its own is booked what its kind's runs used, where
/// it was booked the cold size before: five targets of the kind, one run each with a 1 GiB peak
/// and one core, give a new target of the kind one core and 1.5 GiB (`1.2 x 1 GiB + 128 MiB`
/// rounded up to 256 MiB), and give each of those five targets (one run of their own) the same.
/// The same job without the fallback label, or naming a kind with no runs, is booked from its
/// client's history (every run of the client taught it: the same figures, `sized_from`
/// fallback) and, for a client with no run, the cold size. Catches a fallback label that is not
/// read, a run not taught to its kind, and a client's history that is not the last fallback.
#[test]
fn a_target_with_no_runs_is_booked_from_its_kinds_runs_else_its_clients_before_the_cold_size() {
    let tables = Tables::new();
    let jobs = five_targets(&tables, GIB, Duration::from_secs(10));
    let sizes = taught(&jobs, booked(2000, 4 * GIB));
    let kind_size = (1000, 6 * 256 * MIB);
    let fresh = step("fresh", "fresh");
    assert_eq!(size(&sizes, &fresh), kind_size, "from the kind");
    assert_eq!(sizes.sized_from(&fresh, Day(0)), Some(SizedFrom::Fallback));
    assert_eq!(size(&sizes, &step("t0-again", "t0")), kind_size);
    let unlabelled = queued("unlabelled", of_class("fresh", needs(0, 0)));
    assert_eq!(
        size(&sizes, &unlabelled),
        kind_size,
        "no fallback: the client's"
    );
    assert_eq!(
        sizes.sized_from(&unlabelled, Day(0)),
        Some(SizedFrom::Fallback)
    );
    let other = queued("other", of_kind("fresh", "cargo-test", needs(0, 0)));
    assert_eq!(
        size(&sizes, &other),
        kind_size,
        "another kind: the client's"
    );
    let stranger = queued_by(
        Some("stranger"),
        "stranger",
        of_kind("fresh", KIND, needs(0, 0)),
    );
    assert_eq!(size(&sizes, &stranger), (2000, 4 * GIB), "another client");
    assert_eq!(sizes.sized_from(&stranger, Day(0)), Some(SizedFrom::Cold));
}

/// Asserts how a target's own runs take over from its kind: with a kind whose runs peak at
/// 3 GiB and use three cores (3.75 GiB), a target whose runs peak at 512 MiB and use half a
/// core books its own size (half a core and 1 GiB, the least) from its first run on, not the
/// kind's memory. A target whose runs carry no CPU sample books its own memory and the kind's
/// CPU; with no fallback and no CPU sample anywhere (another client's class of five such runs)
/// it books the cold CPU, not the estimator's two cores. Catches a target held at its kind's
/// largest peak after runs of its own, and a learned CPU replaced by the kind's.
#[test]
fn a_targets_own_runs_size_it_from_its_first_run_and_not_its_kinds_largest_peak() {
    let tables = Tables::new();
    let kind_jobs = five_targets(&tables, 3 * GIB, Duration::from_secs(30));
    let small = target_runs(&tables, "small", 5, (MIB * 512, Duration::from_secs(5)));
    let next = step("small-next", "small");
    let after = |runs: usize| {
        let jobs: Vec<JobRecord> = kind_jobs.iter().chain(&small[..runs]).cloned().collect();
        let sizes = taught(&jobs, booked(2000, 4 * GIB));
        (size(&sizes, &next), sizes.sized_from(&next, Day(0)))
    };
    let kind = 15 * 256 * MIB;
    assert_eq!(
        after(0),
        ((3000, kind), Some(SizedFrom::Fallback)),
        "no run of its own: the kind's"
    );
    for runs in 1..=5 {
        assert_eq!(
            after(runs),
            ((500, GIB), Some(SizedFrom::Class)),
            "{runs} runs: its own"
        );
    }

    let mut jobs = kind_jobs;
    jobs.extend(target_runs(
        &tables,
        "silent",
        2,
        (MIB * 512, Duration::ZERO),
    ));
    let sizes = taught(&jobs, booked(2000, 4 * GIB));
    assert_eq!(size(&sizes, &step("silent-next", "silent")), (3000, GIB));
    let alone = queued("alone", of_class("silent", needs(0, 0)));
    assert_eq!(
        size(&sizes, &alone),
        (3000, GIB),
        "no fallback: the client's CPU, its own memory"
    );

    let tables = Tables::new();
    let mute: Vec<JobRecord> = (0..5)
        .map(|at| {
            ran(
                &tables,
                (&format!("mute-{at}"), Some("mute")),
                of_class("mute", needs(0, 0)),
                booked(2000, 4 * GIB),
                exited(0, MIB * 512, Duration::ZERO),
            )
        })
        .collect();
    let sizes = taught(&mute, booked(2000, 4 * GIB));
    let next = queued_by(Some("mute"), "mute-next", of_class("mute", needs(0, 0)));
    assert_eq!(
        size(&sizes, &next),
        (2000, GIB),
        "settled, no CPU sample: the cold CPU"
    );
}

/// `n` runs of the target `class`, a step of [`KIND`], booked two cores and 4 GiB, each
/// peaking at `peak` bytes and using `cpu` of CPU time in ten seconds; ids `<class>-<n>`.
fn target_runs(
    tables: &Tables,
    class: &str,
    n: usize,
    (peak, cpu): (u64, Duration),
) -> Vec<JobRecord> {
    (0..n)
        .map(|at| {
            let id = format!("{class}-{at}");
            let spec = of_kind(class, KIND, needs(0, 0));
            ran(
                tables,
                (&id, None),
                spec,
                booked(2000, 4 * GIB),
                exited(0, peak, cpu),
            )
        })
        .collect()
}

/// Asserts that a memory kill of a target booked from its kind raises the target to at least
/// twice the booking it died at, and teaches the kind the kill too: with a kind at 1.5 GiB, a
/// target killed at 1.5 GiB books 3 GiB next (its floor, twice the booking it died at, over the
/// kill's own sample of its 1.5 GiB limit, `1.2 x 1.5 GiB + 128 MiB`, 2 GiB), and so does a new
/// target of the kind.
/// Catches a kill floor lost because the size came from the kind, and a kill that teaches
/// only the target (its siblings would die at the same size).
#[test]
fn a_kill_of_a_target_booked_from_its_kind_raises_the_target_and_the_kind() {
    let tables = Tables::new();
    let mut jobs = five_targets(&tables, GIB, Duration::from_secs(10));
    let killed = JobEnd::Memory {
        exit_code: None,
        output: output(6 * 256 * MIB, Duration::from_secs(5)),
    };
    let oom = ran(
        &tables,
        ("oom-run", None),
        of_kind("oom", KIND, needs(0, 0)),
        booked(1000, 6 * 256 * MIB),
        killed,
    );
    let mut sizes = taught(&jobs, booked(2000, 4 * GIB));
    assert!(sizes.observe(&oom, booked(1000, 6 * 256 * MIB)));
    jobs.push(oom);
    let raised = 12 * 256 * MIB;
    assert_eq!(raised, 2 * 6 * 256 * MIB);
    assert_eq!(size(&sizes, &step("oom-next", "oom")), (1000, raised));
    assert_eq!(size(&sizes, &step("sibling", "sibling")), (1000, raised));
}

/// Asserts that a kind teaches a new target from even one run, and that one whose runs reach
/// the host's memory does not: one run, or two, of the kind give a new target the kind's CPU
/// (a core) and memory (1.5 GiB); a kind whose runs reach the host's memory gives its CPU and
/// leaves the memory at the cold size, not the whole host. Catches a kind of one run ignored,
/// and a whole-host booking handed from a kind to a target that may be small.
#[test]
fn a_kind_of_one_run_sizes_a_new_target_and_one_over_the_host_leaves_its_memory_cold() {
    let tables = Tables::new();
    let one = GIB;
    let cpu = Duration::from_secs(10);
    let jobs = kind_runs(&tables, &["a0", "a1"], KIND, (one, cpu));
    let fresh = step("fresh", "fresh");
    let sizes = taught(&jobs[..1], booked(2000, 4 * GIB));
    let kind = (1000, 6 * 256 * MIB);
    assert_eq!(size(&sizes, &fresh), kind, "one run");
    assert_eq!(sizes.sized_from(&fresh, Day(0)), Some(SizedFrom::Fallback));
    let sizes = taught(&jobs, booked(2000, 4 * GIB));
    assert_eq!(size(&sizes, &fresh), kind, "two runs");
    assert_eq!(sizes.sized_from(&fresh, Day(0)), Some(SizedFrom::Fallback));
    let huge = (14 * GIB, cpu);
    let jobs = kind_runs(&tables, &["h0", "h1"], "huge-kind", huge);
    let sizes = taught(&jobs, booked(2000, 16 * GIB));
    let fresh = queued("fresh-huge", of_kind("fresh", "huge-kind", needs(0, 0)));
    assert_eq!(
        size(&sizes, &fresh),
        (1000, 4 * GIB),
        "a kind over the host: its CPU, the cold memory"
    );
    assert_eq!(sizes.sized_from(&fresh, Day(0)), Some(SizedFrom::Cold));
}

/// Asserts that a target whose own runs reach the host's memory books the whole of it with a
/// kind behind it too, at the kind's CPU in place of the cold CPU. Catches a kind's small size
/// handed to a target whose own runs say it needs the whole host.
#[test]
fn a_target_whose_runs_reach_the_host_books_all_of_it_with_a_kind_behind_it() {
    let tables = Tables::new();
    let mut jobs = five_targets(&tables, GIB, Duration::from_secs(10));
    let end = || exited(0, 14 * GIB, Duration::from_secs(10));
    jobs.extend(super::runs(
        &tables,
        "huge",
        2,
        "huge",
        booked(2000, 16 * GIB),
        end,
    ));
    let sizes = taught(&jobs, booked(2000, 16 * GIB));
    let next = step("huge-next", "huge");
    assert_eq!(size(&sizes, &next), (1000, 16 * GIB));
    assert_eq!(sizes.sized_from(&next, Day(0)), Some(SizedFrom::OverCap));
}

/// Asserts that a target whose five runs are over 60 days old (stale: no estimate) is booked
/// from its kind's fresh runs, and from its own on the last day they are fresh. Catches a
/// stale class counted as settled, which books the cold size though its kind knows better.
#[test]
fn a_target_whose_own_runs_are_stale_is_booked_from_its_kind() {
    let tables = Tables::new();
    let end = || exited(0, 2 * GIB, Duration::from_secs(30));
    let mut jobs = super::runs(&tables, "old", 5, "old", booked(2000, 4 * GIB), end);
    tables.set_ms(61 * DAY_MS);
    jobs.extend(five_targets(&tables, GIB, Duration::from_secs(10)));
    let sizes = taught(&jobs, booked(2000, 4 * GIB));
    let next = step("old-next", "old");
    let sized = |day: u32| {
        let sized = sizes.sized(&next, Day(day));
        let js_jobs::Needs::Shape(resources) = sized.spec.needs else {
            panic!("a share is sized as a share");
        };
        let from = sizes.sized_from(&next, Day(day));
        ((resources.cpu_millis(), resources.memory_bytes()), from)
    };
    let own = ((3000, 11 * 256 * MIB), Some(SizedFrom::Class));
    assert_eq!(sized(60), own, "fresh on day 60");
    let kind = ((1000, 6 * 256 * MIB), Some(SizedFrom::Fallback));
    assert_eq!(sized(61), kind, "stale on day 61");
}

/// Asserts that a job whose fallback label names its own class teaches that class once (one
/// sample, not two; and its client's history once), and has no fallback class; and that a fallback class is keyed as the
/// `size-class` of that name is, under the job's own client and platform: the runs of a
/// target named `zig-cc` are the history of the kind `zig-cc`, and another client's or
/// platform's `zig-cc` is another class. Catches a run counted twice (five runs would settle
/// a class on three), and a kind shared across tenants or platforms.
#[test]
fn a_fallback_class_is_the_class_of_that_name_for_the_jobs_client_and_platform() {
    let tables = Tables::new();
    let same = ran(
        &tables,
        ("same", None),
        of_kind("k", "k", needs(0, 0)),
        booked(2000, 4 * GIB),
        exited(0, GIB, Duration::from_secs(10)),
    );
    assert_eq!(JobSizes::fallback_key_of(&same), None);
    let sizes = taught(&[same], booked(2000, 4 * GIB));
    assert_eq!(
        sizes.summaries.len(),
        2,
        "the class and the client's history"
    );
    let job = queued("k", of_class("k", needs(0, 0)));
    let key = JobSizes::key_of(&job);
    assert_eq!(sizes.summaries.get(&key).map(|s| s.samples()), Some(1));
    let client = JobSizes::client_key_of(&job);
    assert_eq!(sizes.summaries.get(&client).map(|s| s.samples()), Some(1));

    let fresh = step("fresh", "fresh");
    let named = queued("named", of_class(KIND, needs(0, 0)));
    assert_eq!(
        JobSizes::fallback_key_of(&fresh),
        Some(JobSizes::key_of(&named))
    );
    assert_eq!(
        JobSizes::fallback_key_of(&named),
        None,
        "no label: no fallback"
    );
    let mut arm = of_kind("fresh", KIND, needs(0, 0));
    arm.constraints.arch = Some("aarch64".to_owned());
    let arm = JobSizes::fallback_key_of(&queued("arm", arm));
    assert_ne!(arm, JobSizes::fallback_key_of(&fresh), "a platform's own");
    let new = NewJob {
        id: Some(JobId::parse("alice").unwrap()),
        spec: of_kind("fresh", KIND, needs(0, 0)),
        priority: 0,
        client: Some(ClientName::parse("alice").unwrap()),
    };
    let alice = tables.jobs.insert_queued(new).unwrap().record;
    let alice = JobSizes::fallback_key_of(&alice);
    assert_ne!(alice, JobSizes::fallback_key_of(&fresh), "a client's own");
}

/// Asserts what [`JobSizes::sized_from`] answers for a job the history does not size: nothing
/// for a job that names both its CPU and its memory, one that books the whole host, and any job
/// before a host is known; and the name each path is logged with. Catches a log line for a
/// job whose size it did not decide.
#[test]
fn only_a_job_the_history_sizes_has_a_source() {
    let mut sizes = JobSizes::with_cold(MODULE_COLD);
    let fresh = step("fresh", "fresh");
    assert_eq!(sizes.sized_from(&fresh, Day(0)), None, "no host known");
    sizes.set_bounds(Some(bounds()));
    assert_eq!(sizes.sized_from(&fresh, Day(0)), Some(SizedFrom::Cold));
    let named = queued("named", of_kind("fresh", KIND, needs(500, GIB)));
    assert_eq!(sizes.sized_from(&named, Day(0)), None);
    let half = queued("half", of_kind("fresh", KIND, needs(500, 0)));
    assert_eq!(sizes.sized_from(&half, Day(0)), Some(SizedFrom::Cold));
    let whole = queued("whole", of_kind("fresh", KIND, js_jobs::Needs::WholeHost));
    assert_eq!(sizes.sized_from(&whole, Day(0)), None);
    let names = [
        SizedFrom::Class,
        SizedFrom::Fallback,
        SizedFrom::Cold,
        SizedFrom::OverCap,
    ]
    .map(SizedFrom::as_str);
    assert_eq!(names, ["class", "fallback", "cold", "over-cap"]);
}

/// What a test's own subscriber writes: every line, kept for [`Lines::take`].
#[derive(Clone, Default)]
pub(super) struct Lines(Arc<Mutex<Vec<u8>>>);

impl io::Write for Lines {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut held = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        held.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Lines {
    type Writer = Lines;

    fn make_writer(&'a self) -> Lines {
        self.clone()
    }
}

impl Lines {
    /// The lines written so far, emptied.
    pub(super) fn take(&self) -> String {
        let mut held = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        String::from_utf8(std::mem::take(&mut *held)).unwrap()
    }
}

/// Installs, once per process, a global subscriber that wants every line and writes it
/// nowhere. Tracing settles whether a line is wanted from the subscribers it knows when a
/// thread first reaches the line; without this one, a test thread with no subscriber can
/// settle the placement line as never wanted before the capturing test reaches it.
pub(super) fn process_sink() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(io::sink)
            .try_init();
    });
}

/// Asserts the pass end to end: five targets of a kind run once each through the passes
/// (the first placed cold, the next four at the kind's one core and 1.5 GiB, from even the
/// one run the kind has; each freed run taught to its kind), and the pass
/// then places a new
/// target of the kind booked one core and 1.5 GiB from the kind's runs, beside a job that
/// names its size and is placed as it names it. Each placement of a job the history sized
/// logs one line naming the job, its host, its class and what sized it (`cold` for the
/// first, `fallback` after), and the job that names its size logs none. Catches a pass
/// that teaches only the target class, so every new target of a build stays cold, and a
/// placement log that is lost, names the wrong source or logs a job the history did not size.
#[test]
fn a_pass_books_a_new_target_from_the_runs_its_kind_had_in_earlier_passes() {
    process_sink();
    let lines = Lines::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(lines.clone())
        .with_ansi(false)
        .without_time()
        .with_target(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let tables = Tables::new();
    let clock = ManualClock::new(Timestamp::from_millis(0));
    let settings = Settings {
        cold: MODULE_COLD,
        ..Settings::default()
    };
    let placer = SchedPlacer::new(settings, clock);
    let caps = BTreeMap::from([("arch".to_owned(), "x86_64".to_owned())]);
    let shape = Shape::new(Resources::new(32_000, 16 * GIB, 0, None).unwrap(), caps);
    placer.engine().host_up(&host("a"), &shape, super::at(0));
    let submit = |id: &str, spec: JobSpec| {
        let new = NewJob {
            id: Some(JobId::parse(id).unwrap()),
            spec,
            priority: 0,
            client: None,
        };
        tables.jobs.insert_queued(new).unwrap();
    };
    for (run, target) in ["t0", "t1", "t2", "t3", "t4"].into_iter().enumerate() {
        submit(target, of_kind(target, KIND, needs(0, 0)));
        assert_eq!(pass(&placer, &tables).len(), 1);
        let expected = if run == 0 {
            booked(2000, 4 * GIB)
        } else {
            booked(1000, 6 * 256 * MIB)
        };
        assert_eq!(allocation(&tables, target), expected, "run {run}");
        finish(&tables, target, GIB, Duration::from_secs(10));
    }
    submit("fresh", of_kind("fresh", KIND, needs(0, 0)));
    submit("named", of_kind("named", KIND, needs(500, GIB)));
    assert_eq!(pass(&placer, &tables).len(), 2);
    assert_eq!(allocation(&tables, "fresh"), booked(1000, 6 * 256 * MIB));
    assert_eq!(allocation(&tables, "named"), booked(500, GIB));
    let logged: Vec<String> = lines
        .take()
        .lines()
        .filter(|line| line.contains("a job the history sized was placed"))
        .map(str::to_owned)
        .collect();
    let line = |job: &str, from: &str| {
        format!(
            " INFO a job the history sized was placed job={job} host=a class={job} \
             sized_from=\"{from}\""
        )
    };
    let expected = [
        line("t0", "cold"),
        line("t1", "fallback"),
        line("t2", "fallback"),
        line("t3", "fallback"),
        line("t4", "fallback"),
        line("fresh", "fallback"),
    ];
    assert_eq!(logged, expected);
}

/// Starts the placed job `id` and ends it after ten seconds with a `peak` and `cpu` of CPU
/// time, freeing it.
fn finish(tables: &Tables, id: &str, peak: u64, cpu: Duration) {
    use js_jobs::{EndReason, JobState};
    use js_state::Clock as _;
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
    let end = exited(0, peak, cpu);
    tables
        .jobs
        .end_and_free(&id, end, EndReason::Exited)
        .unwrap();
}

/// Asserts that a fallback class with an estimate but no CPU sample (its only run a memory
/// kill) gives its memory and the server's cold CPU (half a core here), not the estimator's
/// unlearned two cores. Catches a fallback that books more CPU than the job would have booked
/// cold.
#[test]
fn a_fallback_with_no_cpu_sample_gives_the_cold_cpu() {
    let tables = Tables::new();
    let killed = JobEnd::Memory {
        exit_code: None,
        output: output(2 * GIB, Duration::from_secs(5)),
    };
    let oom = ran(
        &tables,
        ("k0-run", None),
        of_kind("k0", KIND, needs(0, 0)),
        booked(2000, 2 * GIB),
        killed,
    );
    let cold = super::super::ColdSize {
        cpu_millis: 500,
        ..MODULE_COLD
    };
    let mut sizes = JobSizes::with_cold(cold);
    sizes.set_bounds(Some(bounds()));
    assert!(sizes.observe(&oom, booked(2000, 2 * GIB)));
    let fresh = step("fresh", "fresh");
    let (cpu, memory) = size(&sizes, &fresh);
    assert_eq!(cpu, 500, "the cold CPU");
    assert!(memory >= 4 * GIB, "at least the kill's floor");
    assert_eq!(sizes.sized_from(&fresh, Day(0)), Some(SizedFrom::Fallback));
}

/// Asserts that a fallback class idle over 60 days is no fallback: its five runs on day 0
/// size a new target on day 60 and leave it cold on day 61. Catches a stale kind trusted.
#[test]
fn a_stale_fallback_class_is_no_fallback() {
    let tables = Tables::new();
    let jobs = five_targets(&tables, GIB, Duration::from_secs(10));
    let sizes = taught(&jobs, booked(2000, 4 * GIB));
    let fresh = step("fresh", "fresh");
    assert_eq!(sizes.sized_from(&fresh, Day(60)), Some(SizedFrom::Fallback));
    assert_eq!(sizes.sized_from(&fresh, Day(61)), Some(SizedFrom::Cold));
}

/// Asserts that a class whose runs are stale keeps its kill floor when its kind sizes it: a
/// target killed at 4 GiB on day 0 (floor 8 GiB) whose kind, fresh on day 61, books 1.5 GiB
/// books 8 GiB on day 61. Catches a floor dropped on the fallback path.
#[test]
fn a_stale_class_keeps_its_kill_floor_under_its_kind() {
    let tables = Tables::new();
    let killed = JobEnd::Memory {
        exit_code: None,
        output: output(4 * GIB, Duration::from_secs(5)),
    };
    let oom = ran(
        &tables,
        ("old-run", None),
        of_class("old", needs(0, 0)),
        booked(2000, 4 * GIB),
        killed,
    );
    tables.set_ms(61 * DAY_MS);
    let mut jobs = vec![oom];
    jobs.extend(five_targets(&tables, GIB, Duration::from_secs(10)));
    let sizes = taught(&jobs, booked(2000, 4 * GIB));
    let next = step("old-next", "old");
    let Needs::Shape(sized) = sizes.sized(&next, Day(61)).spec.needs else {
        panic!("a share is sized as a share");
    };
    assert_eq!((sized.cpu_millis(), sized.memory_bytes()), (1000, 8 * GIB));
    assert_eq!(sizes.sized_from(&next, Day(61)), Some(SizedFrom::Fallback));
}

/// Asserts that a server's start teaches the kind from the ended jobs the store holds
/// ([`SchedPlacer::learn_ended`], taught before any host is known): after five ended targets
/// of a kind, the first pass places a new target of the kind at the kind's size. Catches a
/// restart that forgets every kind until its targets run again.
#[test]
fn a_start_teaches_each_kind_from_the_ended_jobs_of_its_targets() {
    let tables = Tables::new();
    five_targets(&tables, GIB, Duration::from_secs(10));
    let clock = ManualClock::new(Timestamp::from_millis(0));
    let settings = Settings {
        cold: MODULE_COLD,
        ..Settings::default()
    };
    let placer = SchedPlacer::new(settings, clock);
    assert_eq!(placer.learn_ended(&tables.jobs).unwrap(), 5);
    let caps = BTreeMap::from([("arch".to_owned(), "x86_64".to_owned())]);
    let shape = Shape::new(Resources::new(32_000, 16 * GIB, 0, None).unwrap(), caps);
    placer.engine().host_up(&host("a"), &shape, super::at(0));
    let new = NewJob {
        id: Some(JobId::parse("fresh").unwrap()),
        spec: of_kind("fresh", KIND, needs(0, 0)),
        priority: 0,
        client: None,
    };
    tables.jobs.insert_queued(new).unwrap();
    assert_eq!(pass(&placer, &tables).len(), 1);
    assert_eq!(allocation(&tables, "fresh"), booked(1000, 6 * 256 * MIB));
}
