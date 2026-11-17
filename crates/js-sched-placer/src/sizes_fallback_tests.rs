//! The fallback class ([`SIZE_FALLBACK_LABEL`]): a job whose own class has too few runs is
//! booked from the history of the class it names there (a build step's kind, where its
//! `size-class` is its target) before the cold size, and every run teaches both classes. The
//! host is the module's: eight cores and 16 GiB, where the cold size is two cores and 4 GiB.

use std::collections::BTreeMap;
use std::time::Duration;

use js_jobs::{ClientName, JobEnd, JobId, JobRecord, JobSpec, NewJob};
use js_sizing::Day;
use js_types::{Labels, ManualClock, Resources, Shape, Timestamp};

use super::super::{SIZE_FALLBACK_LABEL, SizedFrom};
use super::{
    GIB, JobSizes, MIB, SIZE_CLASS_LABEL, Tables, allocation, booked, bounds, exited, host, needs,
    of_class, output, pass, queued, ran, size, spec_of, taught,
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
/// The same job without the fallback label, or naming a kind with no runs, is still booked the
/// cold size. Catches a fallback label that is not read, a run not taught to its kind, and a
/// kind's history that sizes jobs naming another kind.
#[test]
fn a_target_with_no_runs_is_booked_from_its_kinds_runs_before_the_cold_size() {
    let tables = Tables::new();
    let jobs = five_targets(&tables, GIB, Duration::from_secs(10));
    let sizes = taught(&jobs, booked(2000, 4 * GIB));
    let kind_size = (1000, 6 * 256 * MIB);
    let fresh = step("fresh", "fresh");
    assert_eq!(size(&sizes, &fresh), kind_size, "from the kind");
    assert_eq!(sizes.sized_from(&fresh, Day(0)), Some(SizedFrom::Fallback));
    assert_eq!(size(&sizes, &step("t0-again", "t0")), kind_size);
    let unlabelled = queued("unlabelled", of_class("fresh", needs(0, 0)));
    assert_eq!(size(&sizes, &unlabelled), (2000, 4 * GIB), "no fallback");
    assert_eq!(sizes.sized_from(&unlabelled, Day(0)), Some(SizedFrom::Cold));
    let other = queued("other", of_kind("fresh", "cargo-test", needs(0, 0)));
    assert_eq!(size(&sizes, &other), (2000, 4 * GIB), "another kind");
}

/// Asserts how a target's own runs take over from its kind: with a kind whose runs peak at
/// 3 GiB and use three cores (3.75 GiB), a target whose runs peak at 512 MiB and use half a
/// core books the kind's size after one run of its own (too few to estimate), the kind's
/// memory and its own CPU after two to four (its memory may only rise from the kind's, as it
/// may only rise from the cold size without one), and its own size (half a core and 1 GiB, the
/// least) from its fifth run on, as without a fallback. A target whose runs carry no CPU
/// sample books the kind's CPU, where without a fallback it books the estimator's two cores.
/// Catches a target held at its kind after five runs, a target lowered below its kind on too
/// few runs, and a learned CPU replaced by the kind's.
#[test]
fn a_targets_own_runs_raise_it_from_its_kind_and_take_over_at_five() {
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
    let fallback = Some(SizedFrom::Fallback);
    assert_eq!(after(1), ((3000, kind), fallback), "one run: the kind's");
    for runs in 2..5 {
        assert_eq!(
            after(runs),
            ((500, kind), fallback),
            "{runs} runs: raised from the kind"
        );
    }
    assert_eq!(
        after(5),
        ((500, GIB), Some(SizedFrom::Class)),
        "five: its own"
    );

    let mut jobs = kind_jobs;
    jobs.extend(target_runs(
        &tables,
        "silent",
        2,
        (MIB * 512, Duration::ZERO),
    ));
    let sizes = taught(&jobs, booked(2000, 4 * GIB));
    assert_eq!(size(&sizes, &step("silent-next", "silent")), (3000, kind));
    let alone = queued("alone", of_class("silent", needs(0, 0)));
    assert_eq!(
        size(&sizes, &alone),
        (2000, 4 * GIB),
        "no fallback: two cores"
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
/// target killed at 1.5 GiB books 3.5 GiB next (the kill is a sample of its 2.75 GiB limit,
/// `1.2 x 2.75 GiB + 128 MiB`, over its 3 GiB floor), and so does a new target of the kind.
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
    let raised = 14 * 256 * MIB;
    assert!(raised >= 2 * 6 * 256 * MIB);
    assert_eq!(size(&sizes, &step("oom-next", "oom")), (1000, raised));
    assert_eq!(size(&sizes, &step("sibling", "sibling")), (1000, raised));
}

/// Asserts that a kind with no estimate is no fallback: one run of the kind leaves a new
/// target at the cold size; two runs (an estimate that may only rise from the cold memory)
/// give the kind's CPU and the cold memory; a kind whose runs reach the host's memory leaves
/// a new target at the cold size, not the whole host. Catches a kind of one run trusted, and
/// a whole-host booking handed from a kind to a target that may be small.
#[test]
fn a_kind_with_no_estimate_leaves_a_new_target_at_the_cold_size() {
    let tables = Tables::new();
    let one = GIB;
    let cpu = Duration::from_secs(10);
    let jobs = kind_runs(&tables, &["a0", "a1"], KIND, (one, cpu));
    let fresh = step("fresh", "fresh");
    let sizes = taught(&jobs[..1], booked(2000, 4 * GIB));
    assert_eq!(size(&sizes, &fresh), (2000, 4 * GIB), "one run: cold");
    assert_eq!(sizes.sized_from(&fresh, Day(0)), Some(SizedFrom::Cold));
    let sizes = taught(&jobs, booked(2000, 4 * GIB));
    assert_eq!(
        size(&sizes, &fresh),
        (1000, 4 * GIB),
        "two runs: raise-only"
    );
    assert_eq!(sizes.sized_from(&fresh, Day(0)), Some(SizedFrom::Fallback));
    let huge = (14 * GIB, cpu);
    let jobs = kind_runs(&tables, &["h0", "h1"], "huge-kind", huge);
    let sizes = taught(&jobs, booked(2000, 16 * GIB));
    let fresh = queued("fresh-huge", of_kind("fresh", "huge-kind", needs(0, 0)));
    assert_eq!(
        size(&sizes, &fresh),
        (2000, 4 * GIB),
        "a kind over the host"
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
/// sample, not two), and has no fallback class; and that a fallback class is keyed as the
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
    assert_eq!(sizes.summaries.len(), 1);
    let key = JobSizes::key_of(&queued("k", of_class("k", needs(0, 0))));
    assert_eq!(sizes.summaries.get(&key).map(|s| s.samples()), Some(1));

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
    let mut sizes = JobSizes::new();
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

/// Asserts the pass end to end: five targets of a kind run once each through the passes
/// (the first two placed cold, the next three at the kind's one core and, the kind having
/// fewer than five runs, the cold memory; each freed run taught to its kind), and the pass
/// then places a new
/// target of the kind booked one core and 1.5 GiB from the kind's runs, beside a job that
/// names its size and is placed as it names it. Catches a pass that teaches only the target
/// class, so every new target of a build stays cold.
#[test]
fn a_pass_books_a_new_target_from_the_runs_its_kind_had_in_earlier_passes() {
    let tables = Tables::new();
    let clock = ManualClock::new(Timestamp::from_millis(0));
    let placer = SchedPlacer::new(Settings::default(), clock);
    let caps = BTreeMap::from([("arch".to_owned(), "x86_64".to_owned())]);
    let shape = Shape::new(Resources::new(8000, 16 * GIB, 0, None).unwrap(), caps);
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
        let cpu = if run < 2 { 2000 } else { 1000 };
        let expected = booked(cpu, 4 * GIB);
        assert_eq!(allocation(&tables, target), expected, "run {run}");
        finish(&tables, target, GIB, Duration::from_secs(10));
    }
    submit("fresh", of_kind("fresh", KIND, needs(0, 0)));
    submit("named", of_kind("named", KIND, needs(500, GIB)));
    assert_eq!(pass(&placer, &tables).len(), 2);
    assert_eq!(allocation(&tables, "fresh"), booked(1000, 6 * 256 * MIB));
    assert_eq!(allocation(&tables, "named"), booked(500, GIB));
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
