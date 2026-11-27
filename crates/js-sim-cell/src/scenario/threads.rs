//! A scenario's seeds on several threads, with the answer of one thread.

use std::ops::Range;
use std::panic::resume_unwind;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

use super::{Passed, Scenario, ScenarioFailure, SeedRun};

/// One finished seed: its number and what it came to.
type Finished = (u64, Result<SeedRun, ScenarioFailure>);

/// Runs the seeds of `seeds` of the scenario `build` makes on `threads` threads, and
/// answers what `build().try_run_seed_range(seeds)` answers: the same [`Passed`] (the
/// runs in seed order), or the failure of the lowest failing seed, and the witnesses
/// judged over every seed of the range, not over a thread's share.
///
/// A scenario is built and run on one thread (it holds `Rc`s), so each thread calls
/// `build` for itself, and the calling thread's scenario also judges the witnesses.
/// `build` must make the same scenario every time. `threads` is clamped to at least one
/// and at most the number of seeds; with one thread the seeds run on the calling thread,
/// as [`Scenario::try_run_seed_range`] runs them.
///
/// The threads take seeds from one counter, lowest first, and every seed of the range
/// runs, so the failure answered is the lowest failing seed's, as the sequential run's
/// is; the sequential run stops at its first failure and this does not.
///
/// # Errors
///
/// The first failure, as [`Scenario::try_run_seed_range`] reports it.
///
/// # Panics
///
/// When a seed panics on a spawned thread, with that panic.
pub fn try_run_seed_range_on_threads(
    build: impl Fn() -> Scenario + Sync,
    seeds: Range<u64>,
    threads: usize,
) -> Result<Passed, ScenarioFailure> {
    let scenario = build();
    let count = seeds.end.saturating_sub(seeds.start);
    let most = usize::try_from(count).unwrap_or(usize::MAX).max(1);
    let workers = threads.clamp(1, most);
    if workers == 1 {
        return scenario.try_run_seed_range(seeds);
    }
    let end = seeds.end;
    let next = AtomicU64::new(seeds.start);
    let work = |scenario: &Scenario| {
        let mut done: Vec<Finished> = Vec::new();
        loop {
            let seed = next.fetch_add(1, Ordering::SeqCst);
            if seed >= end {
                break done;
            }
            done.push((seed, scenario.run_seed(seed)));
        }
    };
    let (work, build) = (&work, &build);
    let mut finished = thread::scope(|scope| {
        let mut spawned = Vec::new();
        for _ in 1..workers {
            spawned.push(scope.spawn(move || work(&build())));
        }
        let mut finished = work(&scenario);
        for handle in spawned {
            match handle.join() {
                Ok(done) => finished.extend(done),
                Err(panicked) => resume_unwind(panicked),
            }
        }
        finished
    });
    finished.sort_by_key(|(seed, _)| *seed);
    let runs = finished.into_iter().map(|(_, ran)| ran);
    let runs = runs.collect::<Result<Vec<SeedRun>, ScenarioFailure>>()?;
    scenario.judged(count, runs)
}

/// [`try_run_seed_range_on_threads`], failing the test: the [`Passed`], or a panic with
/// the first failure as text, as [`Scenario::run_seed_range`] does.
///
/// # Panics
///
/// With the [`ScenarioFailure`] as text, at the first failure.
pub fn run_seed_range_on_threads(
    build: impl Fn() -> Scenario + Sync,
    seeds: Range<u64>,
    threads: usize,
) -> Passed {
    try_run_seed_range_on_threads(build, seeds, threads)
        .unwrap_or_else(|failure| panic!("{failure}"))
}
