//! The profiles a class learns in shadow ([`Curves`], [`JobSizes::with_curves`]): the runs of
//! the job tables on the in-memory fake, each with the series its host recorded, every clock the
//! test's. What a run is checked against is read from the line a run freed while serving logs.

use std::collections::BTreeMap;
use std::io;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use js_jobs::{Booked, CpuThrottle, JobEnd, JobId, JobRecord, NewJob, Output};
use js_sizing::{Basis, Day, Instance, Key, PlatformClass, Shape};
use js_types::sched::Resources as Capacity;
use js_types::{CpuSeries, ManualClock, MemorySeries, Timestamp};

use super::{ClassProfile, Curves, MAX_PROFILES, evict_for};
use crate::engine::rig::{Tables, at};
use crate::engine::{Engine, Settings};
use crate::place::SchedPlacer;
use crate::sizes::tests::{GIB, booked, exited, needs, of_class, ran};
use crate::sizes::{JobSizes, SizedFrom, bounds_of};

/// The start of every line a checked run logs.
const PREFIX: &str = " INFO a run was checked against its class's profile";

/// The message of the line that says the shadow checks start.
const START: &str =
    " INFO placement curves: the shadow checks start, the profiles learned from the ended jobs";

/// What a test's own subscriber writes: every line, kept for [`Lines::take`].
#[derive(Clone, Default)]
pub(crate) struct Lines(Arc<Mutex<Vec<u8>>>);

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
    /// Every line written so far, emptied.
    pub(crate) fn taken(&self) -> Vec<String> {
        let mut held = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let text = String::from_utf8(std::mem::take(&mut *held)).unwrap();
        text.lines().map(str::to_owned).collect()
    }

    /// The lines of a checked run written so far, emptied.
    fn checked(&self) -> Vec<String> {
        let mut held = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let text = String::from_utf8(std::mem::take(&mut *held)).unwrap();
        text.lines()
            .filter(|line| line.contains("a run was checked against its class's profile"))
            .map(str::to_owned)
            .collect()
    }

    /// The check lines and the lines saying the checks start, written so far, in order,
    /// emptied.
    fn profile_lines(&self) -> Vec<String> {
        let mut held = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let text = String::from_utf8(std::mem::take(&mut *held)).unwrap();
        text.lines()
            .filter(|line| line.starts_with(PREFIX) | line.starts_with(START))
            .map(str::to_owned)
            .collect()
    }
}

/// Installs, once per process, a global subscriber that wants every line and writes it
/// nowhere. Tracing settles whether a line is wanted from the subscribers it knows when a
/// thread first reaches the line; without this one, a test thread with no subscriber can
/// settle the line as never wanted before the capturing test reaches it.
fn process_sink() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(io::sink)
            .try_init();
    });
}

/// A subscriber that keeps every line this test's thread logs, until the guard drops.
pub(crate) fn capture() -> (Lines, tracing::subscriber::DefaultGuard) {
    subscribed(false)
}

/// [`capture`], each line written with its target.
pub(crate) fn capture_targets() -> (Lines, tracing::subscriber::DefaultGuard) {
    subscribed(true)
}

/// A subscriber that keeps every line this test's thread logs, with its target when `target`
/// says so.
fn subscribed(target: bool) -> (Lines, tracing::subscriber::DefaultGuard) {
    process_sink();
    let lines = Lines::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(lines.clone())
        .with_ansi(false)
        .without_time()
        .with_target(target)
        .finish();
    (lines, tracing::subscriber::set_default(subscriber))
}

/// What every run of this module booked.
fn bk() -> Booked {
    booked(2000, 4 * GIB)
}

/// A history that profiles by `curves`, with the cold size of two cores and 4 GiB.
fn sizes_with(curves: Curves) -> JobSizes {
    let cold = crate::sizes::ColdSize {
        cpu_millis: 2000,
        memory_bytes: 4 * GIB,
    };
    JobSizes::with_cold(cold).with_curves(curves)
}

/// A CPU series of `points` sixteenths of a core, one second a point.
fn cpu_series(points: &[u16], partial: bool) -> CpuSeries {
    let run_usec = u64::try_from(points.len()).unwrap() * 1_000_000;
    CpuSeries::new(1, points.to_vec(), run_usec, partial).unwrap()
}

/// A memory series of `points` MiB, one second a point.
fn memory_series(points: &[u32], partial: bool) -> MemorySeries {
    let run_usec = u64::try_from(points.len()).unwrap() * 1_000_000;
    MemorySeries::new(1, points.to_vec(), run_usec, partial).unwrap()
}

/// The run of `class` recorded as `id`, ended and freed in `tables` ten seconds after the last,
/// that exited with `code` and recorded `cpu` and `memory`; its CPU time is what its CPU series
/// holds (ten seconds without one), and `tweak` changes what the host reported before it ends.
fn run_with(
    tables: &Tables,
    (id, class): (&str, &str),
    code: i32,
    (cpu, memory): (Option<CpuSeries>, Option<MemorySeries>),
    tweak: impl FnOnce(&mut Output),
) -> JobRecord {
    // A sixteenth of a core for a second is 62 500 microseconds.
    let cpu_time = cpu.as_ref().map_or(Duration::from_secs(10), |series| {
        let sixteenths: u64 = series.points.iter().map(|point| u64::from(*point)).sum();
        Duration::from_micros(sixteenths * 62_500)
    });
    let JobEnd::Exited { mut output, .. } = exited(code, GIB, cpu_time) else {
        panic!("an exit");
    };
    output.cpu_series = cpu;
    output.memory_series = memory;
    tweak(&mut output);
    let end = JobEnd::Exited {
        exit_code: code,
        output,
    };
    ran(tables, (id, None), of_class(class, needs(0, 0)), bk(), end)
}

/// A successful run of `class` whose CPU series is `points` and which recorded no memory series.
fn good(tables: &Tables, id: &str, class: &str, points: &[u16]) -> JobRecord {
    let series = Some(cpu_series(points, false));
    run_with(tables, (id, class), 0, (series, None), |_| {})
}

/// A 30 second series that is idle but for a burst of `burst` sixteenths of a core from the
/// 20th second to the 24th.
fn phased(burst: u16) -> Vec<u16> {
    let mut points = vec![0; 30];
    points[20..24].fill(burst);
    points
}

/// The profiles of `job`'s class in `sizes`.
fn class<'a>(sizes: &'a JobSizes, job: &JobRecord) -> &'a ClassProfile {
    &sizes.profiles.as_ref().unwrap().classes[&JobSizes::key_of(job)]
}

/// Asserts that the server's three words name the three modes, that anything else names none,
/// and that the mode is off by default. Catches a flag that takes another spelling, and a mode
/// that does not round-trip through its word.
#[test]
fn the_modes_are_named_off_and_shadow_and_off_is_the_default() {
    assert_eq!(Curves::default(), Curves::Off);
    let parsed = ["off", "shadow", "on", "", "Shadow", "off "].map(Curves::parse);
    assert_eq!(
        parsed,
        [
            Some(Curves::Off),
            Some(Curves::Shadow),
            Some(Curves::On),
            None,
            None,
            None
        ]
    );
    assert_eq!(
        [Curves::Off, Curves::Shadow, Curves::On].map(Curves::as_str),
        ["off", "shadow", "on"]
    );
}

/// Asserts that a history with the profiles off keeps none and logs no check for a run freed
/// while serving, and that the default history keeps none. Catches profiles learned when the
/// server did not ask for them.
#[test]
fn off_learns_nothing_and_logs_nothing() {
    let (lines, _guard) = capture();
    let tables = Tables::new();
    let job = good(&tables, "a0", "a", &[16; 10]);
    let mut off = sizes_with(Curves::Off);
    off.freed_run(job.clone(), bk());
    assert!(off.profiles.is_none());
    assert!(JobSizes::new().profiles.is_none());
    assert!(lines.checked().is_empty());
}

/// Asserts that a history with the profiles in shadow learns a pair of profiles for each class,
/// and logs for each run freed while serving one line that says what the run did against what
/// its class's profile had: the first run of a class has no level to be checked against, the
/// second is checked against the first, and another class starts its own. Catches runs pooled
/// across classes, a line for a run that is not freed, and a line that lacks the prediction.
#[test]
fn shadow_learns_each_class_and_logs_one_line_per_run_freed() {
    let (lines, _guard) = capture();
    let tables = Tables::new();
    let both = |id: &str, class: &str, cpu: u16, memory: u32| {
        let series = (
            Some(cpu_series(&[cpu; 10], false)),
            Some(memory_series(&[memory; 10], false)),
        );
        run_with(&tables, (id, class), 0, series, |_| {})
    };
    let jobs = [
        both("a0", "a", 16, 1024),
        both("a1", "a", 16, 1024),
        both("b0", "b", 32, 2048),
    ];
    let mut sizes = sizes_with(Curves::Shadow);
    for job in &jobs {
        sizes.freed_run(job.clone(), bk());
    }
    let expected = [
        format!(
            "{PREFIX} job=a0 class=a cpu=\"learned\" cpu_runs=1 cpu_shaped=false \
             cpu_mean_millicores=1000 cpu_peak_millicores=1000 memory=\"learned\" \
             memory_runs=1 memory_shaped=false memory_mean_mib=1024 memory_peak_mib=1024"
        ),
        format!(
            "{PREFIX} job=a1 class=a cpu=\"learned\" cpu_runs=2 cpu_shaped=false \
             cpu_mean_millicores=1000 cpu_level_millicores=1000 cpu_peak_millicores=1000 \
             memory=\"learned\" memory_runs=2 memory_shaped=false memory_mean_mib=1024 \
             memory_level_mib=1024 memory_peak_mib=1024"
        ),
        format!(
            "{PREFIX} job=b0 class=b cpu=\"learned\" cpu_runs=1 cpu_shaped=false \
             cpu_mean_millicores=2000 cpu_peak_millicores=2000 memory=\"learned\" \
             memory_runs=1 memory_shaped=false memory_mean_mib=2048 memory_peak_mib=2048"
        ),
    ];
    assert_eq!(lines.checked(), expected);
    assert_eq!(sizes.profiles.as_ref().unwrap().classes.len(), 2);
    let a = class(&sizes, &jobs[0]);
    assert_eq!((a.cpu.runs(), a.memory.runs()), (2, 2));
    let b = class(&sizes, &jobs[2]);
    assert_eq!((b.cpu.runs(), b.memory.runs()), (1, 1));
}

/// Asserts that a run observed from the history (what [`JobSizes::observe`] teaches) is learned
/// and not logged. Catches a restart that logs a line for every run of the last seven days.
#[test]
fn a_run_observed_from_the_history_is_learned_and_not_logged() {
    let (lines, _guard) = capture();
    let tables = Tables::new();
    let mut sizes = sizes_with(Curves::Shadow);
    let job = good(&tables, "h0", "h", &[16; 10]);
    assert!(sizes.observe(&job, bk()));
    assert_eq!(class(&sizes, &job).cpu.runs(), 1);
    assert!(lines.checked().is_empty());
}

/// Asserts what a start teaches: the ended jobs the store holds are learned without a line, and
/// the run a pass freed while the teaching read them is learned after them with its line, which
/// is checked against all the ended runs. Catches a teaching that logs the history, or that
/// drops the held run's line.
#[test]
fn a_teaching_logs_the_runs_it_held_and_not_the_ended_jobs_it_read() {
    let (lines, _guard) = capture();
    let tables = Tables::new();
    good(&tables, "h0", "h", &[16; 10]);
    let first = good(&tables, "h1", "h", &[16; 10]);
    let placer = SchedPlacer::new(
        Settings {
            curves: Curves::Shadow,
            ..Settings::default()
        },
        ManualClock::new(Timestamp::from_millis(0)),
    );
    let mut teaching = placer.begin_teaching(&tables.jobs, 0).unwrap();
    assert!(!teaching.read_page(&tables.jobs).unwrap());
    let held = good(&tables, "h2", "h", &[16; 10]);
    placer.engine().sizes().freed_run(held, bk());
    assert!(lines.checked().is_empty(), "a held run logs when taught");
    placer.teach(teaching);
    let logged = lines.checked();
    assert_eq!(logged.len(), 1, "{logged:?}");
    assert!(logged[0].starts_with(&format!("{PREFIX} job=h2 class=h ")));
    assert!(logged[0].contains("cpu_runs=3 "), "{logged:?}");
    let mut engine = placer.engine();
    assert_eq!(class(engine.sizes(), &first).cpu.runs(), 3);
}

/// A placer whose engine profiles by `curves`, on `clock`.
fn placer_with(curves: Curves, clock: &ManualClock) -> SchedPlacer {
    let settings = Settings {
        curves,
        ..Settings::default()
    };
    SchedPlacer::new(settings, clock.clone())
}

/// Asserts the line a start's teaching logs in shadow: nothing while the ended jobs are read and
/// a run the passes free is held; at the teaching, one line naming the two classes the three
/// ended jobs made, the three jobs and the 300 whole seconds the placer's clock moved since the
/// teaching began (300.9 s), then the held run's check; and a run a pass frees after it logs its
/// check alone. Catches a start whose checks begin with no line, a line logged before the ended
/// jobs are taught or after the held runs, and a line logged again by a later pass.
#[test]
fn a_teaching_in_shadow_logs_once_that_the_checks_start_before_the_held_runs() {
    let (lines, _guard) = capture();
    let tables = Tables::new();
    good(&tables, "h0", "h", &[16; 10]);
    good(&tables, "h1", "h", &[16; 10]);
    good(&tables, "g0", "g", &[16; 10]);
    let clock = ManualClock::new(Timestamp::from_millis(1_000));
    let placer = placer_with(Curves::Shadow, &clock);
    let mut teaching = placer.begin_teaching(&tables.jobs, 0).unwrap();
    assert!(!teaching.read_page(&tables.jobs).unwrap());
    let held = good(&tables, "h2", "h", &[16; 10]);
    placer.engine().sizes().freed_run(held, bk());
    assert_eq!(lines.profile_lines(), Vec::<String>::new());
    clock.advance(Duration::from_millis(300_900));
    placer.teach(teaching);
    let logged = lines.profile_lines();
    assert_eq!(logged.len(), 2, "{logged:?}");
    assert_eq!(
        logged[0],
        format!("{START} classes=2 ended_jobs=3 took_secs=300")
    );
    assert!(
        logged[1].starts_with(&format!("{PREFIX} job=h2 class=h ")),
        "{logged:?}"
    );
    let after = good(&tables, "h3", "h", &[16; 10]);
    placer.engine().sizes().freed_run(after, bk());
    let logged = lines.profile_lines();
    assert_eq!(logged.len(), 1, "{logged:?}");
    assert!(
        logged[0].starts_with(&format!("{PREFIX} job=h3 class=h ")),
        "{logged:?}"
    );
}

/// Asserts the line of a teaching taught before its read finished (a stop or a failed read):
/// it names no class, no ended job and no time, since the clock did not move, and the checks
/// start all the same. Catches a line left out when nothing was read, and ended jobs counted
/// that were not taught.
#[test]
fn a_teaching_not_read_through_logs_that_the_checks_start_from_nothing() {
    let (lines, _guard) = capture();
    let tables = Tables::new();
    good(&tables, "h0", "h", &[16; 10]);
    let clock = ManualClock::new(Timestamp::from_millis(0));
    let placer = placer_with(Curves::Shadow, &clock);
    let teaching = placer.begin_teaching(&tables.jobs, 0).unwrap();
    placer.teach(teaching);
    let expected = vec![format!("{START} classes=0 ended_jobs=0 took_secs=0")];
    assert_eq!(lines.profile_lines(), expected);
}

/// Asserts that with the profiles off a teaching logs neither that the checks start nor any
/// check, read through or not. Catches the line logged where no check follows it.
#[test]
fn a_teaching_with_the_profiles_off_logs_no_start() {
    let (lines, _guard) = capture();
    let tables = Tables::new();
    good(&tables, "h0", "h", &[16; 10]);
    let clock = ManualClock::new(Timestamp::from_millis(0));
    let placer = placer_with(Curves::Off, &clock);
    let mut teaching = placer.begin_teaching(&tables.jobs, 0).unwrap();
    assert!(!teaching.read_page(&tables.jobs).unwrap());
    let held = good(&tables, "h1", "h", &[16; 10]);
    placer.engine().sizes().freed_run(held, bk());
    clock.advance(Duration::from_secs(60));
    placer.teach(teaching);
    placer.teach(placer.begin_teaching(&tables.jobs, 0).unwrap());
    assert_eq!(lines.profile_lines(), Vec::<String>::new());
}

/// Asserts that a run that did not succeed teaches no profile and logs `not-a-success` for both
/// dimensions, and starts no class. Catches a failed run learned as a sample of what the class
/// uses.
#[test]
fn a_run_that_did_not_succeed_is_not_learned() {
    let (lines, _guard) = capture();
    let tables = Tables::new();
    let series = (
        Some(cpu_series(&[16; 10], false)),
        Some(memory_series(&[1024; 10], false)),
    );
    let failed = run_with(&tables, ("f0", "f"), 1, series, |_| {});
    let mut sizes = sizes_with(Curves::Shadow);
    sizes.freed_run(failed, bk());
    assert_eq!(
        lines.checked(),
        [format!(
            "{PREFIX} job=f0 class=f cpu=\"not-a-success\" memory=\"not-a-success\""
        )]
    );
    assert!(sizes.profiles.as_ref().unwrap().classes.is_empty());
}

/// Asserts that a series the learner cannot trust is skipped and says why: none recorded, one
/// that misses the run's start (for CPU and for memory), a CPU series whose total is far from
/// the run's exact CPU time, and a run with no exact CPU time; the dimension with a whole
/// series is learned beside the skipped one. Catches a partial series learned as a run with an
/// idle start, and a CPU series learned that the run's own CPU time contradicts.
#[test]
fn a_series_that_is_missing_partial_or_disagreeing_is_skipped() {
    let (lines, _guard) = capture();
    let tables = Tables::new();
    let mut sizes = sizes_with(Curves::Shadow);
    let whole_memory = || Some(memory_series(&[1024; 10], false));
    let skipped = [
        run_with(&tables, ("s0", "s"), 0, (None, None), |_| {}),
        run_with(
            &tables,
            ("s1", "s"),
            0,
            (
                Some(cpu_series(&[16; 10], true)),
                Some(memory_series(&[1024; 10], true)),
            ),
            |_| {},
        ),
        run_with(
            &tables,
            ("s2", "s"),
            0,
            (Some(cpu_series(&[16; 10], false)), whole_memory()),
            |output| output.usage.as_mut().unwrap().cpu_time = Duration::from_secs(20),
        ),
        run_with(
            &tables,
            ("s3", "s"),
            0,
            (Some(cpu_series(&[16; 10], false)), whole_memory()),
            |output| output.usage = None,
        ),
    ];
    for job in &skipped {
        sizes.freed_run(job.clone(), bk());
    }
    let logged = lines.checked();
    let statuses = [
        r#"cpu="no-series""#,
        r#"cpu="partial""#,
        r#"cpu="disagrees""#,
        r#"cpu="disagrees""#,
    ];
    let memory = [
        r#"memory="no-series""#,
        r#"memory="partial""#,
        r#"memory="learned""#,
        r#"memory="learned""#,
    ];
    assert_eq!(logged.len(), 4);
    for ((line, cpu), memory) in logged.iter().zip(statuses).zip(memory) {
        assert!(line.contains(cpu) && line.contains(memory), "{line}");
        assert!(!line.contains("cpu_runs"), "{line}");
    }
    let profile = class(&sizes, &skipped[0]);
    assert_eq!((profile.cpu.runs(), profile.memory.runs()), (0, 2));
}

/// Asserts that a run slowed by contention (its group waited for a CPU in half its wall time) or
/// held at a ceiling (throttled in every period) teaches its class's CPU only when it used more
/// than the class's level, and that a clean run is always learned, below the level too; an
/// unclean run of a class with no run teaches nothing. Catches a slowed run that lowers its
/// class's profile, and one that is never learned.
#[test]
fn a_run_slowed_by_contention_or_held_at_a_ceiling_may_only_raise_its_class() {
    let (lines, _guard) = capture();
    let tables = Tables::new();
    let mut sizes = sizes_with(Curves::Shadow);
    let contended = |output: &mut Output| {
        output.usage.as_mut().unwrap().cpu_pressure_some = Some(Duration::from_secs(5));
    };
    let held = |output: &mut Output| {
        output.usage.as_mut().unwrap().cpu_throttle = Some(CpuThrottle {
            periods: 100,
            throttled_periods: 100,
        });
    };
    let cpu = |points: &[u16]| (Some(cpu_series(points, false)), None);
    let steady: Vec<JobRecord> = (0..3)
        .map(|at| good(&tables, &format!("c{at}"), "c", &[16; 10]))
        .collect();
    let runs = [
        run_with(&tables, ("c3", "c"), 0, cpu(&[8; 10]), contended),
        run_with(&tables, ("c4", "c"), 0, cpu(&[8; 10]), held),
        run_with(&tables, ("c5", "c"), 0, cpu(&[32; 10]), contended),
        run_with(&tables, ("c6", "c"), 0, cpu(&[8; 10]), |_| {}),
        run_with(&tables, ("e0", "e"), 0, cpu(&[16; 10]), contended),
    ];
    for job in steady.iter().chain(&runs) {
        sizes.freed_run(job.clone(), bk());
    }
    let logged = lines.checked();
    let learned = [
        (3, r#"cpu="not-learned" cpu_runs=3 "#),
        (4, r#"cpu="not-learned" cpu_runs=3 "#),
        (5, r#"cpu="learned" cpu_runs=4 "#),
        (6, r#"cpu="learned" cpu_runs=5 "#),
        (7, r#"cpu="not-learned" cpu_runs=0 "#),
    ];
    for (at, status) in learned {
        assert!(logged[at].contains(status), "{}", logged[at]);
    }
    assert_eq!(class(&sizes, &steady[0]).cpu.runs(), 5);
    assert_eq!(class(&sizes, &runs[4]).cpu.runs(), 0);
}

/// Asserts what a run is checked against once its class has eight runs: nine runs of a class
/// that is idle but for a burst from the 20th second to the 24th, the ninth with a burst half
/// as high again, log the ninth's level (4 sixteenths, 250 millicores), its mean and peak, the
/// largest 90th percentile the profile had over the 2 second slots it reached (the burst's
/// 32 sixteenths, 2000 millicores), and the error of the curve (64) against the error of the
/// flat level (280), as millicore-seconds, with the share of the two. Catches a check made
/// before eight runs, or one that does not log what the profile predicted.
#[test]
fn a_run_checked_against_eight_runs_logs_its_predicted_level_and_peak_and_both_errors() {
    let (lines, _guard) = capture();
    let tables = Tables::new();
    let mut sizes = sizes_with(Curves::Shadow);
    for at in 0..8 {
        let job = good(&tables, &format!("p{at}"), "p", &phased(32));
        sizes.freed_run(job, bk());
    }
    let ninth = good(&tables, "p8", "p", &phased(48));
    sizes.freed_run(ninth, bk());
    let logged = lines.checked();
    assert!(!logged[7].contains("cpu_curve_error"), "{}", logged[7]);
    assert_eq!(
        logged[8],
        format!(
            "{PREFIX} job=p8 class=p cpu=\"learned\" cpu_runs=9 cpu_shaped=false \
             cpu_mean_millicores=375 cpu_level_millicores=250 cpu_peak_millicores=3000 \
             cpu_predicted_peak_millicores=2000 cpu_curve_error=4000 cpu_flat_error=17500 \
             cpu_fit_permille=228 memory=\"no-series\""
        )
    );
}

/// Asserts that a class whose runs are all idle but for a burst at the same seconds turns shaped
/// when 16 runs have been scored (the 24th run), not before, with the curve's error nil against
/// the flat level's; and that a class whose runs are the same all through but differ from run to
/// run (16 and 32 sixteenths of a core in turn) stays flat, its curve's error the flat level's
/// (1000 permille). Catches a class that is shaped too early, and one that is shaped though its
/// curve predicts no better than its level.
#[test]
fn a_phased_class_turns_shaped_after_sixteen_scored_runs_and_a_flat_one_does_not() {
    let (lines, _guard) = capture();
    let tables = Tables::new();
    let mut sizes = sizes_with(Curves::Shadow);
    let phased_jobs: Vec<JobRecord> = (0..24)
        .map(|at| good(&tables, &format!("p{at:02}"), "phased", &phased(32)))
        .collect();
    let flat_jobs: Vec<JobRecord> = (0..24)
        .map(|at| {
            let level = 16 * (1 + at % 2);
            good(&tables, &format!("f{at:02}"), "flat", &[level; 10])
        })
        .collect();
    for job in phased_jobs.iter().chain(&flat_jobs) {
        sizes.freed_run(job.clone(), bk());
    }
    let logged = lines.checked();
    assert!(logged[22].contains("cpu_shaped=false"), "{}", logged[22]);
    assert!(logged[23].contains("cpu_shaped=true"), "{}", logged[23]);
    assert!(logged[23].contains("cpu_fit_permille=0"), "{}", logged[23]);
    assert!(logged[47].contains("cpu_shaped=false"), "{}", logged[47]);
    assert!(
        logged[47].contains("cpu_fit_permille=1000"),
        "{}",
        logged[47]
    );
    let phased_class = class(&sizes, &phased_jobs[0]);
    assert!(phased_class.cpu.shaped());
    assert_eq!(phased_class.cpu.window().fit_permille(), Some(0));
    let flat_class = class(&sizes, &flat_jobs[0]);
    assert!(!flat_class.cpu.shaped());
    assert_eq!(flat_class.cpu.window().fit_permille(), Some(1000));
}

/// Asserts that a class's `last_ended_ms` is the end of its latest run whatever order the runs
/// are observed in. Catches an eviction order that follows the order runs were read.
#[test]
fn a_class_remembers_the_end_of_its_latest_run() {
    let tables = Tables::new();
    let early = good(&tables, "l0", "l", &[16; 10]);
    let late = good(&tables, "l1", "l", &[16; 10]);
    let mut sizes = sizes_with(Curves::Shadow);
    assert!(sizes.observe(&late, bk()));
    assert!(sizes.observe(&early, bk()));
    let ended_ms = |job: &JobRecord| {
        let ended = job.result.as_ref().unwrap().timing.ended_at.as_duration();
        u64::try_from(ended.as_millis()).unwrap()
    };
    assert!(ended_ms(&early) < ended_ms(&late));
    assert_eq!(class(&sizes, &early).last_ended_ms, ended_ms(&late));
}

/// The key of the class of hash `n`.
fn key(n: u64) -> Key {
    Key::new(
        Instance::new(""),
        PlatformClass::new("/".to_owned()),
        Basis::Cgroup,
        Shape(n),
        None,
    )
}

/// Asserts the room a class makes: a class already held, or any class while the map is under its
/// cap, evicts nothing; a new class at the cap evicts the class whose last run ended earliest,
/// the smallest key of those that tie; a cap of nothing evicts nothing from an empty map; and
/// the server's cap is 2048 classes. Catches an eviction of the class being fed, one of the
/// newest class, a tie broken by the largest key, and a map that grows without a bound.
#[test]
fn a_new_class_at_the_cap_evicts_the_class_whose_last_run_ended_earliest() {
    assert_eq!(MAX_PROFILES, 2048);
    let mut classes = BTreeMap::new();
    for (n, ended) in [(5, 10), (1, 10), (7, 20)] {
        let mut class = ClassProfile::new();
        class.last_ended_ms = ended;
        classes.insert(key(n), class);
    }
    let held =
        |classes: &BTreeMap<Key, ClassProfile>| -> Vec<Key> { classes.keys().cloned().collect() };
    evict_for(&mut classes, &key(5), 3);
    evict_for(&mut classes, &key(9), 4);
    assert_eq!(held(&classes), [key(1), key(5), key(7)]);
    evict_for(&mut classes, &key(9), 3);
    assert_eq!(held(&classes), [key(5), key(7)], "the tie: the smaller key");
    evict_for(&mut classes, &key(9), 2);
    assert_eq!(held(&classes), [key(7)], "the earlier end");
    let mut none = BTreeMap::new();
    evict_for(&mut none, &key(1), 0);
    assert!(none.is_empty());
}

/// Asserts that the sizes booked are the same with the profiles off, in shadow and on: after the
/// same five runs (each with its series) the next job of the class is booked the same, from the
/// same history, and each run teaches the same. Catches a profile that changes what a class
/// books.
#[test]
fn the_sizes_booked_are_the_same_with_the_profiles_off_and_in_shadow() {
    let tables = Tables::new();
    let jobs: Vec<JobRecord> = (0..5)
        .map(|at| {
            let series = (
                Some(cpu_series(&[48; 10], false)),
                Some(memory_series(&[2048; 10], false)),
            );
            run_with(&tables, (&format!("u{at}"), "u"), 0, series, |_| {})
        })
        .collect();
    let queued = Tables::new();
    let new = NewJob {
        id: Some(JobId::parse("next").unwrap()),
        spec: of_class("u", needs(0, 0)),
        priority: 0,
        client: None,
    };
    let next = queued.jobs.insert_queued(new).unwrap().record;
    let bounds = bounds_of([Capacity::new(32_000, 16 * GIB)]);
    let mut books = Vec::new();
    for curves in [Curves::Off, Curves::Shadow, Curves::On] {
        let mut sizes = sizes_with(curves);
        sizes.set_bounds(bounds);
        for job in &jobs {
            assert!(sizes.observe(job, bk()));
        }
        books.push((sizes.sized(&next, Day(0)), sizes.sized_from(&next, Day(0))));
    }
    assert_eq!(books[0], books[1]);
    assert_eq!(books[0], books[2]);
    assert_ne!(books[0].0, next, "the class's runs size it");
    assert_eq!(books[0].1, Some(SizedFrom::Class));
}

/// Asserts that the engine keeps profiles only for the setting that asks for them (shadow and
/// on). Catches a setting that never reaches the history.
#[test]
fn the_engine_builds_its_history_with_the_curves_setting() {
    for (curves, kept) in [
        (Curves::Off, false),
        (Curves::Shadow, true),
        (Curves::On, true),
    ] {
        let settings = Settings {
            curves,
            ..Settings::default()
        };
        let mut engine = Engine::build(settings, at(0));
        assert_eq!(engine.sizes().profiles.is_some(), kept, "{curves:?}");
    }
}
