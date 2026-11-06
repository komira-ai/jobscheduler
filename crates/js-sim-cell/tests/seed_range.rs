//! Seeds past the first few: `Scenario::run_seed_range` over a range of seeds, and the sweep
//! (`js_sim_cell::sweep`) that runs a family over one shard of a range, each shown to fail
//! when it should: the seeds a range runs, a failing seed and its report in the panic, the
//! shards of a sweep, flags that do not fit together, a run that is not a sweep, and each
//! kind of failure printed with the command that replays it.
//!
//! Sizes: one host beating at the server's default for at most a few minutes, a few
//! hundred agent requests a seed, over at most ten seeds a test.

use std::cell::RefCell;
use std::ffi::OsString;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::process::ExitCode;
use std::rc::Rc;
use std::time::Duration;

use js_sim::{Chance, Faults};
use js_sim_cell::actions::set_bus_faults;
use js_sim_cell::sweep::{self, COMMAND};
use js_sim_cell::{
    Action, Cell, Counter, Expect, HowEnded, Scenario, ScenarioFailure, ServerSpec, Shape, Work,
};

/// One host and one run of 10 s, settled 120 s after its last action, under `name`.
fn small(name: &str) -> Scenario {
    Scenario::new(name)
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(ServerSpec::registered_pool("burst", Shape::gpu_host(), 1))
        .hosts("burst", 1)
        .work(1, Work::run_for(Duration::from_secs(10)))
        .settle_within(Duration::from_secs(120))
}

/// An action that writes the seed it ran in: the seeds a scenario ran, as the cell saw them.
#[derive(Clone, Default)]
struct Seeds(Rc<RefCell<Vec<u64>>>);

impl Seeds {
    fn ran(&self) -> Vec<u64> {
        self.0.borrow().clone()
    }
}

impl Action for Seeds {
    fn run(&self, cell: &mut Cell) {
        self.0.borrow_mut().push(cell.seed());
    }
}

/// An action that, in seed `seed` only, makes one message in ten on the agent wire lost
/// from then on, so that seed cannot pass `settles-when-quiet`.
struct LossyIn(u64);

impl Action for LossyIn {
    fn run(&self, cell: &mut Cell) {
        if cell.seed() == self.0 {
            let lossy = Faults {
                drop: Chance::percent(10),
                ..Faults::default()
            };
            set_bus_faults(lossy).run(cell);
        }
    }
}

/// `small(name)`, recording its seeds in `seeds`, and failing in seed `fails` only.
fn failing_in(name: &str, seeds: &Seeds, fails: u64) -> Scenario {
    small(name)
        .at(Duration::from_secs(1), seeds.clone())
        .at(Duration::from_secs(2), LossyIn(fails))
}

/// What a panic carried, as text.
fn message(panic: &(dyn std::any::Any + Send)) -> String {
    let text = panic.downcast_ref::<String>().cloned();
    text.unwrap_or_else(|| {
        panic
            .downcast_ref::<&str>()
            .map(|s| (*s).to_owned())
            .unwrap_or_default()
    })
}

/// Asserts that `run_seed_range(3..6)` runs seeds 3, 4 and 5, once each and in that order,
/// as the cell's own seed reads in each, and returns their runs in that order; and that an
/// empty range runs no seed. Catches a range run from seed 0, one that skips its first or
/// runs past its last seed, and an empty range that runs one.
#[test]
fn run_seed_range_runs_exactly_the_seeds_of_its_range() {
    let seeds = Seeds::default();
    let passed = small("run_seed_range_runs_its_seeds")
        .at(Duration::from_secs(1), seeds.clone())
        .run_seed_range(3..6);
    assert_eq!(seeds.ran(), [3, 4, 5]);
    let ran: Vec<u64> = passed.runs.iter().map(|run| run.seed).collect();
    assert_eq!(ran, [3, 4, 5]);
    let none = Seeds::default();
    let passed = small("an_empty_range")
        .at(Duration::from_secs(1), none.clone())
        .run_seed_range(5..5);
    assert!(passed.runs.is_empty());
    assert!(none.ran().is_empty());
}

/// Asserts that `run_seeds(n)` is `run_seed_range(0..n)`: the same seeds, with the same
/// trace hashes, counts and outcomes. Catches `run_seeds` starting anywhere but seed 0, or
/// running its seeds otherwise than a range does.
#[test]
fn run_seeds_is_run_seed_range_from_seed_zero() {
    let scenario = small("run_seeds_is_a_range_from_zero");
    let seeds = scenario.run_seeds(3);
    let range = scenario.run_seed_range(0..3);
    let key = |runs: &[js_sim_cell::SeedRun]| -> Vec<String> {
        let each = runs.iter();
        each.map(|run| {
            format!(
                "{} {} {:?} {:?}",
                run.seed, run.hash, run.counters, run.outcomes
            )
        })
        .collect()
    };
    assert_eq!(key(&seeds.runs), key(&range.runs));
    assert_eq!(seeds.runs.len(), 3);
}

/// Asserts that a scenario that fails in seed 4 only, run over 3..6, panics with the
/// failure as text: the heading naming the scenario and seed 4, then the whole failure
/// report (the hashes and the last forty trace lines), equal to what
/// `try_run_seed_range` returns; and that it stopped at seed 4, so seed 5 never ran.
/// Catches a panic that loses the seed or the report, and a range that runs on past its
/// first failure.
#[test]
fn run_seed_range_fails_its_test_with_the_failing_seed_and_its_report() {
    let name = "a_scenario_that_fails_in_seed_four";
    let seeds = Seeds::default();
    let scenario = failing_in(name, &seeds, 4);
    let panicked = catch_unwind(AssertUnwindSafe(|| scenario.run_seed_range(3..6)));
    let panic = panicked.expect_err("seed 4 fails");
    assert_eq!(
        seeds.ran(),
        [3, 4, 4],
        "seed 4 ran again with its trace kept"
    );
    let text = message(&*panic);
    let failure = scenario.try_run_seed_range(3..6).expect_err("seed 4 fails");
    let ScenarioFailure::Violated(report) = &failure else {
        panic!("not a violation: {failure}");
    };
    assert_eq!(report.seed, 4);
    assert_eq!(text, failure.to_string());
    let heading = format!("scenario {name} seed 4 failed at check {}: ", report.check);
    assert!(text.starts_with(&heading), "{text}");
    assert!(text.contains("settles-when-quiet"), "{text}");
    assert!(text.contains("\nlast 40 trace lines:\n"), "{text}");
}

/// The sweep's exit status, what it wrote to stdout and what it wrote to stderr, for the
/// flags `flags` over `family`.
fn swept(family: &[Scenario], flags: &[&str]) -> (String, String, String) {
    let args = std::iter::once("sweep").chain(flags.iter().copied());
    let args: Vec<OsString> = args.map(OsString::from).collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = sweep::run(family, args, &mut out, &mut err);
    (
        format!("{code:?}"),
        String::from_utf8(out).expect("utf-8"),
        String::from_utf8(err).expect("utf-8"),
    )
}

/// An exit status as [`swept`] gives it.
fn status(code: ExitCode) -> String {
    format!("{code:?}")
}

/// Asserts that a run with none of the sweep's flags, as `cargo test` runs the binary with
/// libtest's arguments or none, runs no seed, passes, and says why on stderr only. Catches
/// a sweep that runs the family under every plain `cargo test`, or that fails it.
#[test]
fn a_run_with_no_sweep_flag_runs_no_seed_and_says_why() {
    let seeds = Seeds::default();
    let family = [small("not_swept").at(Duration::from_secs(1), seeds.clone())];
    let (code, out, err) = swept(&family, &["--nocapture", "a_filter"]);
    assert_eq!(code, status(ExitCode::SUCCESS));
    assert_eq!(out, "");
    assert_eq!(
        err,
        "sweep: no sweep flag (--start, --count, --shards, --shard, --scenario), so no seed \
         runs; arguments: [--nocapture a_filter]\n"
    );
    assert!(seeds.ran().is_empty());
}

/// Asserts that the shards of a sweep of ten seeds from seed 7 split into four run seeds
/// 7..17, each seed in exactly one shard, in order, the shards of two or three seeds; that
/// each shard's heading names its seeds, and its line the seeds that passed; and that the
/// flags may be written `--flag=value`. Catches a shard that overlaps the next, leaves a
/// gap, or starts from seed 0.
#[test]
fn the_shards_of_a_sweep_cover_its_seeds_once_in_order() {
    let mut all = Vec::new();
    let mut sizes = Vec::new();
    for shard in 0..4 {
        let seeds = Seeds::default();
        let family = [small("sharded").at(Duration::from_secs(1), seeds.clone())];
        let shard = shard.to_string();
        let flags = [
            "--start=7",
            "--count",
            "10",
            "--shards",
            "4",
            "--shard",
            &shard,
        ];
        let (code, out, err) = swept(&family, &flags);
        assert_eq!((code, err.as_str()), (status(ExitCode::SUCCESS), ""));
        let ran = seeds.ran();
        let (first, last) = (ran[0], ran[ran.len() - 1] + 1);
        assert_eq!(
            out,
            format!(
                "sweep: seeds {first}..{last}, shard {shard} of 4 of seeds 7..17\n\
                 scenario sharded: {} seeds passed\n",
                ran.len()
            )
        );
        sizes.push(ran.len());
        all.extend(ran);
    }
    assert_eq!(all, (7..17).collect::<Vec<u64>>());
    assert_eq!(sizes, [2, 3, 2, 3]);
}

/// Asserts that flags that do not fit together are refused, naming why, with exit status 2
/// and no seed run: no seed, no shard, a shard past the last, fewer seeds than shards, a range past
/// the last seed, an unknown flag, `--count` missing, and a scenario the family does not
/// have; and that `--help` prints the usage on stdout and passes. Catches a sweep that
/// runs a wrong range, or no range, and passes.
#[test]
fn flags_that_do_not_fit_together_are_refused() {
    let seeds = Seeds::default();
    let family = [
        small("refused_one").at(Duration::from_secs(1), seeds.clone()),
        small("refused_two"),
    ];
    let max = u64::MAX.to_string();
    let refused = [
        (vec!["--count", "0"], "--count must be at least 1"),
        (
            vec!["--count", "4", "--shards", "0"],
            "--shards must be at least 1",
        ),
        (
            vec!["--count", "4", "--shards", "2", "--shard", "2"],
            "--shard 2 is not below --shards 2 (shards count from 0)",
        ),
        (
            vec!["--count", "3", "--shards", "4"],
            "--count 3 is fewer than --shards 4, so a shard would run no seed",
        ),
        (
            vec!["--start", &max, "--count", "1"],
            "--start 18446744073709551615 and --count 1 run past the last seed, 18446744073709551615",
        ),
        (
            vec!["--count", "1", "--scenario", "absent"],
            "no scenario absent; the family has refused_one, refused_two",
        ),
    ];
    for (flags, why) in refused {
        let (code, out, err) = swept(&family, &flags);
        assert_eq!(code, status(ExitCode::from(2)), "{flags:?}");
        assert_eq!(
            (out.as_str(), err),
            ("", format!("sweep: {why}\n")),
            "{flags:?}"
        );
    }
    for flags in [vec!["--count", "1", "--bogus"], vec!["--start", "3"]] {
        let (code, out, err) = swept(&family, &flags);
        assert_eq!(code, status(ExitCode::from(2)), "{flags:?}");
        assert_eq!(out, "", "{flags:?}");
        assert!(err.starts_with("error: "), "{flags:?}: {err}");
    }
    let (code, out, err) = swept(&family, &["--count", "1", "--help"]);
    assert_eq!((code, err.as_str()), (status(ExitCode::SUCCESS), ""));
    assert!(
        out.contains("Usage: sweep [OPTIONS] --count <COUNT>"),
        "{out}"
    );
    assert!(seeds.ran().is_empty());
}

/// Asserts that a sweep over seeds 2..5 prints, for each scenario that fails, its failure
/// as text and then the replay command, and goes on to the next: a violation in seed 3
/// (the whole report) and an expectation unmet and a failure to start, each replayed at its
/// own seed alone; a witness no seed of the range satisfied, replayed over the whole range;
/// that the scenario that passes says so; that the run exits 1; and that `--scenario` runs
/// that one alone. Catches a failure printed without its seed, its report or a command that
/// reruns it, and a sweep that stops at its first failing scenario.
#[test]
fn each_failure_prints_its_text_and_the_command_that_replays_it() {
    let never = |c: &js_sim_cell::Counters| c.get(Counter::StartAfterStop) >= 1_000;
    let family = [
        failing_in("violated", &Seeds::default(), 3),
        small("unmet").expect(Expect::terminal_count(1, HowEnded::Cancelled)),
        Scenario::new("unstarted"),
        small("unwitnessed").witness("never", never),
        small("passes"),
    ];
    let (code, out, err) = swept(&family, &["--start", "2", "--count", "3"]);
    assert_eq!((code, err.as_str()), (status(ExitCode::FAILURE), ""));
    let failed = |scenario: &Scenario| scenario.try_run_seed_range(2..5).expect_err("fails");
    let replay = |name: &str, start: u64, count: u64| {
        format!("replay: {COMMAND} --scenario {name} --start {start} --count {count}\n")
    };
    let expected = [
        "sweep: seeds 2..5, shard 0 of 1 of seeds 2..5\n".to_owned(),
        format!("{}\n", failed(&family[0])),
        replay("violated", 3, 1),
        format!("{}\n", failed(&family[1])),
        replay("unmet", 2, 1),
        format!("{}\n", failed(&family[2])),
        replay("unstarted", 2, 1),
        format!("{}\n", failed(&family[3])),
        replay("unwitnessed", 2, 3),
        "scenario passes: 3 seeds passed\n".to_owned(),
    ];
    assert_eq!(out, expected.concat());
    assert!(
        out.contains("scenario violated seed 3 failed at check "),
        "{out}"
    );
    let (code, out, _) = swept(
        &family,
        &["--scenario", "passes", "--start", "2", "--count", "3"],
    );
    assert_eq!(code, status(ExitCode::SUCCESS));
    assert_eq!(
        out,
        "sweep: seeds 2..5, shard 0 of 1 of seeds 2..5\nscenario passes: 3 seeds passed\n"
    );
}

/// The heading a sweep of `flags` prints over a family whose one scenario cannot start (so
/// it runs its first seed only, and that at once), and the replay line's first seed.
fn heading(flags: &[&str]) -> (String, String) {
    let (code, out, err) = swept(&[Scenario::new("unstarted")], flags);
    assert_eq!((code, err.as_str()), (status(ExitCode::FAILURE), ""));
    let first = out.lines().next().unwrap_or_default().to_owned();
    let replay = out.lines().last().unwrap_or_default();
    let start = replay.split("--start ").nth(1).unwrap_or_default();
    (
        first,
        start.split(' ').next().unwrap_or_default().to_owned(),
    )
}

/// Asserts the shards at the edges of the seeds: a range that ends at the last seed
/// (`u64::MAX` is the end, never a seed) is taken; as many seeds as shards gives each shard
/// one; the shards of all `u64::MAX` seeds split them without overflow, each starting where
/// the one before ended; and `--shard=N` and `--scenario=name` are read as flags. Catches
/// a range off by one at the top, shard arithmetic that overflows, and a flag missed when
/// written with `=`.
#[test]
fn the_shards_at_the_edges_of_the_seeds() {
    let max = u64::MAX;
    let last = (max - 1).to_string();
    assert_eq!(
        heading(&["--start", &last, "--count", "1"]),
        (
            format!("sweep: seeds {last}..{max}, shard 0 of 1 of seeds {last}..{max}"),
            last.clone()
        )
    );
    assert_eq!(
        heading(&[
            "--start",
            "10",
            "--count",
            "3",
            "--shards",
            "3",
            "--shard=2"
        ]),
        (
            "sweep: seeds 12..13, shard 2 of 3 of seeds 10..13".to_owned(),
            "12".to_owned()
        )
    );
    let all = max.to_string();
    let (one, two) = (max / 3, max / 3 * 2);
    for (shard, from, to) in [("0", 0, one), ("1", one, two), ("2", two, max)] {
        let flags = [
            "--count",
            &all,
            "--shards",
            "3",
            "--shard",
            shard,
            "--scenario=unstarted",
        ];
        assert_eq!(
            heading(&flags),
            (
                format!("sweep: seeds {from}..{to}, shard {shard} of 3 of seeds 0..{max}"),
                from.to_string()
            )
        );
    }
}

/// Asserts that `--help` alone, or `-h`, prints the sweep's usage on stdout and passes,
/// running no seed: it is how the flags are found. Catches a `--help` taken for a run
/// that is not a sweep.
#[test]
fn help_alone_prints_the_usage() {
    let seeds = Seeds::default();
    let family = [small("helped").at(Duration::from_secs(1), seeds.clone())];
    for help in ["--help", "-h"] {
        let (code, out, err) = swept(&family, &[help]);
        assert_eq!(
            (code, err.as_str()),
            (status(ExitCode::SUCCESS), ""),
            "{help}"
        );
        assert!(
            out.contains("Usage: sweep [OPTIONS] --count <COUNT>"),
            "{help}: {out}"
        );
    }
    assert!(seeds.ran().is_empty());
}
