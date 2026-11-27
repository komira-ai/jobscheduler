//! The x2 run: an export's real per-minute series replayed through the compared rankers,
//! and the report of what each did. Reported, not asserted: nothing here is a pass or a
//! fail of a ranker.
//!
//! # What is real
//!
//! * The truth: [`SeriesTruth`] burns each run's own minute series (each second of minute
//!   `m` at minute `m`'s mean cores) and holds its own working set of the minute, so the
//!   books close exactly: a run that completes burns its row's CPU time.
//! * The beliefs: the learner's ([`Export::beliefs`]), from each key's earlier runs only,
//!   turned into the replay's [`Belief`] by [`replay_beliefs`].
//! * The arrivals, when the export records starts (the header says so).
//!
//! The pool is invented: [`X2Config::pool`], all hosts of one size.
//!
//! # The rankers
//!
//! In the order of the design's sim x row, plus FW5:
//!
//! | label | rule | beliefs |
//! |---|---|---|
//! | 1 least-loaded | the previous rule, [`LeastLoaded`] | curves |
//! | 2 random | [`Random`] under the seed | curves |
//! | 3, 4, 5 | [`ScoreRule`] of [`RankerKind::Linear`], [`RankerKind::Flat`], [`RankerKind::Score`] | curves |
//! | FW5 | [`ScoreRule`] of [`RankerKind::FlatScore`] | every curve replaced by its flat charge ([`flatten`]), in the predicate too |
//!
//! FW5 is the flat best fit with the curves taken out everywhere (what a scheduler that
//! books one flat number per lease would do with the same score), so "5 over FW5" is the
//! gain from curves over flat; "5 over 4" is the gain of the curve's shape in the fit alone,
//! with the predicate charging curves for both.
//!
//! # The report
//!
//! [`X2Report::render`]: the export's header, the pool and the flags, then per ranker
//! [`Metrics::render`], the shares of the rule that decided each placement, the books and
//! the loop bounds, and the guards a run must pass to say anything
//! ([`Guards::check`]: a failure prints `UNINFORMATIVE`, it does not stop the report). Then
//! the gain lines (5 over FW5, 5 over 4, 5 over 1), the difference of each figure, and the
//! decides-differently guard of 5 against FW5 and against 4.

use std::fmt::Write as _;

use js_sched::ScoreConfig;

use super::export::Export;
use super::fixture::Row;
use super::metrics::{Guards, LoopBounds, Metrics, Unbalanced, nearest_rank};
use super::model::{Belief, Beliefs, Charge, Pool, Truth};
use super::predictor::Predictor;
use super::rankers::{Gd, RankerKind, percent, ratio_ppm};
use super::replay::{Config, Replay, replay};
use super::rules::{LeastLoaded, Random, Ranker};
use super::score_rule::ScoreRule;

const MINUTE_MS: u64 = 60_000;
const MIB: u64 = 1 << 20;

/// The share of placements that must differ for the decides-differently guard.
pub const GD_DIFFER_PPM: u64 = 10_000;

/// What really happens to each run of an export: its own minute series.
#[derive(Clone, Debug)]
pub struct SeriesTruth {
    /// Per lease: core-ms burnt by the end of each minute (entry `m` is minutes `0..m`),
    /// one more entry than the run has minutes.
    burnt: Vec<Vec<u64>>,
    cores_milli: Vec<Vec<u32>>,
    ws_mib: Vec<Vec<u64>>,
    wall_ms: Vec<u64>,
}

/// Core-ms of `cores_milli` over `ms` milliseconds, rounded down.
fn core_ms(cores_milli: u32, ms: u64) -> u64 {
    u64::from(cores_milli) * ms / 1_000
}

impl SeriesTruth {
    /// The truth of `export`'s runs, lease `i` the `i`-th in start order.
    pub fn new(export: &Export) -> Self {
        let mut truth = Self {
            burnt: Vec::new(),
            cores_milli: Vec::new(),
            ws_mib: Vec::new(),
            wall_ms: Vec::new(),
        };
        for i in export.order() {
            let run = &export.profile.runs[i];
            let mut burnt = vec![0u64];
            for (m, cores) in run.cores_milli.iter().enumerate() {
                let span = run
                    .wall_ms
                    .saturating_sub(m as u64 * MINUTE_MS)
                    .min(MINUTE_MS);
                let last = burnt[m];
                burnt.push(last + core_ms(*cores, span));
            }
            truth.burnt.push(burnt);
            truth.cores_milli.push(run.cores_milli.clone());
            truth
                .ws_mib
                .push(run.ws_bytes.iter().map(|b| b.div_ceil(MIB)).collect());
            truth.wall_ms.push(run.wall_ms);
        }
        truth
    }

    /// Core-ms `lease` has burnt `ms` milliseconds into its run (at most its wall).
    fn burnt_by(&self, lease: usize, ms: u64) -> u64 {
        let ms = ms.min(self.wall_ms[lease]);
        let minute = usize::try_from(ms / MINUTE_MS).unwrap_or(usize::MAX);
        let cores = &self.cores_milli[lease];
        let burnt = &self.burnt[lease];
        // `ms` is at most the wall, so its minute is at most the run's last.
        let minute = minute.min(cores.len() - 1);
        burnt[minute] + core_ms(cores[minute], ms - minute as u64 * MINUTE_MS)
    }
}

impl Truth for SeriesTruth {
    fn core_ms(&self, lease: usize, age_tick: u64) -> u64 {
        let from = age_tick.saturating_mul(1_000);
        self.burnt_by(lease, from.saturating_add(1_000)) - self.burnt_by(lease, from)
    }

    fn working_set_mib(&self, lease: usize, age_tick: u64) -> u64 {
        let ws = &self.ws_mib[lease];
        let minute = usize::try_from(age_tick / 60).map_or(ws.len() - 1, |m| m.min(ws.len() - 1));
        ws[minute]
    }
}

/// The beliefs the replay reads, one per lease.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BeliefTable(pub Vec<Belief>);

impl Beliefs for BeliefTable {
    fn belief(&self, lease: usize) -> &Belief {
        &self.0[lease]
    }
}

/// The learner's beliefs as the replay's: the flag, `L`, `p_bar` and the curves, the last
/// point held past the end (the learner's [`super::predictor::Tail::Held`]).
pub fn replay_beliefs(predictor: &Predictor) -> BeliefTable {
    BeliefTable(
        (0..predictor.len())
            .map(|i| {
                let b = predictor.belief(i);
                let (cores, mem) = b.curves();
                let curve = cores
                    .iter()
                    .zip(mem)
                    .map(|(c, m)| Charge::new(*c, *m))
                    .collect();
                Belief::new(
                    b.is_cold(),
                    b.life_minutes(),
                    Charge::new(b.p_bar_cores_milli(), b.p_bar_mem_mib()),
                    curve,
                )
            })
            .collect(),
    )
}

/// Every belief with its curve replaced by its flat charge at every minute and `L = 1`; the
/// cold flag is kept.
pub fn flatten(beliefs: &BeliefTable) -> BeliefTable {
    BeliefTable(
        beliefs
            .0
            .iter()
            .map(|b| Belief::new(b.cold, 1, b.p_bar, Vec::new()))
            .collect(),
    )
}

/// What the run is made with beyond the export.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct X2Config {
    /// The invented pool.
    pub pool: Pool,
    /// The score's balance `b` (`--placement-score-balance`).
    pub balance: u32,
    /// The score's slack tolerance, ppm of capacity (`--placement-score-slack-tol-ppm`).
    pub slack_tol_ppm: u64,
    /// The score's key share, ppm (`--placement-key-share`).
    pub key_share_ppm: u64,
    /// The random ranker's seed.
    pub seed: u64,
    /// Logical minutes before the metric window opens.
    pub warm_up_min: u64,
}

impl Default for X2Config {
    /// [`Pool::full`], the scorer's default flags (balance 1, tolerance 1%, key share 25%),
    /// seed 1 and the replay's warm-up.
    fn default() -> Self {
        Self {
            pool: Pool::full(),
            balance: 1,
            slack_tol_ppm: 10_000,
            key_share_ppm: 250_000,
            seed: 1,
            warm_up_min: Config::new(Pool::full()).warm_up_min,
        }
    }
}

impl X2Config {
    /// The score's flags; each score ranker takes its own fit. `None` when the balance is
    /// above the scorer's range.
    pub fn score(&self) -> Option<ScoreConfig> {
        ScoreConfig::new(self.balance, self.slack_tol_ppm, self.key_share_ppm)
    }
}

/// One ranker's replay.
#[derive(Clone, Debug)]
pub struct RankerRun {
    /// The report's label.
    pub label: &'static str,
    /// What the replay produced.
    pub replay: Replay,
    /// The score ranker, for the decides-differently guard; `None` for rankers 1 and 2.
    pub score: Option<ScoreRule>,
    /// Whether its fit may score more than `64 x` its candidates (ranker 4).
    pub inexact_evaluations: bool,
}

impl RankerRun {
    /// The replay's books.
    pub fn books(&self) -> Result<(), Unbalanced> {
        self.replay.metrics.check_books()
    }

    /// The engine's loop bounds.
    pub fn loop_bounds(&self, hosts: usize) -> Result<(), LoopBounds> {
        self.replay
            .metrics
            .counters
            .check_loop_bounds(hosts, !self.inexact_evaluations)
    }
}

/// The x2 run of an export: one replay per ranker.
#[derive(Clone, Debug)]
pub struct X2Report {
    /// The export's header ([`Export::header`]).
    pub header: String,
    /// What it was run with.
    pub config: X2Config,
    /// The runs, in the order of the module's table.
    pub runs: Vec<RankerRun>,
}

/// The labels of the runs, in order.
pub const LABELS: [&str; 6] = [
    "1 least-loaded",
    "2 random",
    "3 area-only",
    "4 flat (key 5 on window means)",
    "5 score",
    "FW5 score on flat beliefs",
];

/// Replays `export` through every ranker of the module's table under `config`.
///
/// # Panics
/// If `config`'s balance is out of the scorer's range ([`parse_args`] refuses one).
pub fn run(export: &Export, config: &X2Config) -> X2Report {
    let score = config
        .score()
        .expect("the balance is in the scorer's range");
    let predictor = export.beliefs();
    let header = export.header(&predictor);
    let rows: Vec<Row> = export.rows();
    let truth = SeriesTruth::new(export);
    let curves = replay_beliefs(&predictor);
    let flat = flatten(&curves);
    let keys: Vec<_> = rows.iter().map(|r| r.key).collect();
    let mut cfg = Config::new(config.pool);
    cfg.warm_up_min = config.warm_up_min;

    let mut runs = Vec::with_capacity(LABELS.len());
    let once = |ranker: &mut dyn Ranker, beliefs: &BeliefTable| {
        replay(&cfg, &rows, &truth, beliefs, ranker)
    };
    let least = once(&mut LeastLoaded, &curves);
    runs.push(RankerRun {
        label: LABELS[0],
        replay: least,
        score: None,
        inexact_evaluations: false,
    });
    let random = once(&mut Random::new(config.seed), &curves);
    runs.push(RankerRun {
        label: LABELS[1],
        replay: random,
        score: None,
        inexact_evaluations: false,
    });
    let scored = [
        (LABELS[2], RankerKind::Linear, &curves),
        (LABELS[3], RankerKind::Flat, &curves),
        (LABELS[4], RankerKind::Score, &curves),
        (LABELS[5], RankerKind::FlatScore, &flat),
    ];
    for (label, kind, beliefs) in scored {
        let mut rule = ScoreRule::new(kind, score, keys.clone());
        let replay = once(&mut rule, beliefs);
        runs.push(RankerRun {
            label,
            replay,
            score: Some(rule),
            inexact_evaluations: kind == RankerKind::Flat,
        });
    }
    X2Report {
        header,
        config: *config,
        runs,
    }
}

/// A figure of a run the gain lines compare: its name and its value, `None` when the run
/// has no sample of it.
fn figures(m: &Metrics) -> Vec<(&'static str, Option<u64>)> {
    vec![
        (
            "p99 host-window CPU ppm",
            nearest_rank(&m.window_ppm, 99, 100),
        ),
        ("host-windows over 1.1", Some(m.windows_above(1_100_000))),
        ("host-windows over 1.5", Some(m.windows_above(1_500_000))),
        ("wait p95 ms", m.wait_pct(95, 100)),
        ("completed", Some(m.counters.completed)),
        ("killed", Some(m.counters.killed)),
        ("queued at end", Some(m.counters.queued_at_end)),
        ("stranded(a) ppm", Some(m.stranded_a_rate_ppm())),
        ("empty host-minutes", Some(m.empty_host_minutes)),
    ]
}

fn signed(after: u64, before: u64) -> String {
    let d = i128::from(after) - i128::from(before);
    if d > 0 {
        format!("+{d}")
    } else {
        d.to_string()
    }
}

impl X2Report {
    /// The run labelled `label`.
    ///
    /// # Panics
    /// If no run has that label.
    pub fn run(&self, label: &str) -> &RankerRun {
        self.runs
            .iter()
            .find(|r| r.label == label)
            .unwrap_or_else(|| panic!("no run labelled {label}"))
    }

    /// The gain lines of `a` over `b`: each figure of the two and `a`'s difference.
    pub fn gain(&self, a: &str, b: &str) -> String {
        let (ra, rb) = (self.run(a), self.run(b));
        let mut s = String::new();
        let _ = writeln!(s, "gain {a} over {b}:");
        let fa = figures(&ra.replay.metrics);
        let fb = figures(&rb.replay.metrics);
        for ((name, va), (_, vb)) in fa.into_iter().zip(fb) {
            let shown = |v: Option<u64>| v.map_or_else(|| "-".to_string(), |v| v.to_string());
            let delta = match (va, vb) {
                (Some(x), Some(y)) => signed(x, y),
                _ => "-".to_string(),
            };
            let _ = writeln!(s, "  {name}: {} vs {} ({delta})", shown(va), shown(vb));
        }
        s
    }

    /// The decides-differently guard of score ranker `x` against score ranker `y`.
    pub fn guard(&self, x: &str, y: &str) -> String {
        let (Some(rx), Some(ry)) = (&self.run(x).score, &self.run(y).score) else {
            return format!("GD {x} vs {y}: not score rankers\n");
        };
        let gd = Gd::of(rx.ranker(), ry.ranker());
        let verdict = match gd.check(GD_DIFFER_PPM) {
            Ok(()) => "informative".to_string(),
            Err(why) => format!("UNINFORMATIVE: {why}"),
        };
        format!("{}; {verdict}\n", gd.line(&format!("{x} vs {y}")))
    }

    /// The report: see the module doc.
    pub fn render(&self) -> String {
        let c = &self.config;
        let hosts = c.pool.hosts;
        let mut s = self.header.clone();
        let _ = writeln!(
            s,
            "pool: {hosts} hosts of {} millicores, {} MiB, {} leases (invented)",
            c.pool.cap.cpu_millis, c.pool.cap.mem_mib, c.pool.cap.leases
        );
        let _ = writeln!(
            s,
            "score flags: balance {} slack tol {} ppm key share {} ppm; random seed {}; warm-up {} min",
            c.balance, c.slack_tol_ppm, c.key_share_ppm, c.seed, c.warm_up_min
        );
        for r in &self.runs {
            let m = &r.replay.metrics;
            let _ = writeln!(s, "\n== {} ==", r.label);
            s.push_str(&m.render());
            let placed = m.counters.placed;
            let shares: Vec<String> = m
                .decided
                .iter()
                .map(|(by, n)| format!("{by:?} {n} ({})", percent(ratio_ppm(*n, placed))))
                .collect();
            let _ = writeln!(s, "decided by: {}", shares.join(", "));
            let _ = writeln!(
                s,
                "books: {:?}; loop bounds: {:?}",
                r.books(),
                r.loop_bounds(hosts)
            );
            match Guards::default().check(m) {
                Ok(()) => s.push_str("guards: pass\n"),
                Err(failed) => {
                    let _ = writeln!(s, "UNINFORMATIVE: {failed:?}");
                }
            }
        }
        s.push('\n');
        s.push_str(&self.gain(LABELS[4], LABELS[5]));
        s.push_str(&self.gain(LABELS[4], LABELS[3]));
        s.push_str(&self.gain(LABELS[4], LABELS[0]));
        s.push_str(&self.guard(LABELS[4], LABELS[5]));
        s.push_str(&self.guard(LABELS[4], LABELS[3]));
        s
    }
}

/// What the runner is given: the export and the flags of the run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Args {
    /// The export directory.
    pub export: std::path::PathBuf,
    /// The run's settings.
    pub config: X2Config,
}

/// Reads the runner's flags: `--export <dir>` once (required), and at most once each
/// `--hosts`, `--threads`, `--mem-mib` (the pool; [`Pool::full`]'s figures unless given),
/// `--balance`, `--slack-tol-ppm`, `--key-share-ppm` (the score's flags; the scorer's
/// defaults unless given) and `--seed`.
pub fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Args, String> {
    const NUMBERS: [&str; 7] = [
        "--hosts",
        "--threads",
        "--mem-mib",
        "--balance",
        "--slack-tol-ppm",
        "--key-share-ppm",
        "--seed",
    ];
    let mut export = None;
    let mut numbers: [Option<u64>; 7] = [None; 7];
    while let Some(arg) = args.next() {
        if arg == "--export" {
            let value = args.next().ok_or("--export needs a directory")?;
            if export.replace(std::path::PathBuf::from(value)).is_some() {
                return Err("--export given twice".to_string());
            }
            continue;
        }
        let Some(at) = NUMBERS.iter().position(|n| *n == arg) else {
            return Err(format!("unknown argument {arg:?}"));
        };
        let value = args.next().ok_or_else(|| format!("{arg} needs a number"))?;
        let value: u64 = value
            .parse()
            .map_err(|_| format!("{arg} is not a whole number: {value:?}"))?;
        if numbers[at].replace(value).is_some() {
            return Err(format!("{arg} given twice"));
        }
    }
    let export = export.ok_or_else(|| "--export is required".to_string())?;
    let full = Pool::full();
    let [hosts, threads, mem_mib, balance, tol, share, seed] = numbers;
    let hosts = usize::try_from(hosts.unwrap_or(full.hosts as u64))
        .map_err(|_| "--hosts is too large".to_string())?;
    if hosts == 0 {
        return Err("--hosts must be at least 1".to_string());
    }
    let threads = threads.unwrap_or(full.cap.cpu_millis / 1_000);
    if threads == 0 || threads > u64::from(u32::MAX / 4) {
        return Err("--threads must be 1 to 1073741823".to_string());
    }
    let base = X2Config::default();
    let balance = u32::try_from(balance.unwrap_or(u64::from(base.balance)))
        .map_err(|_| "--balance is too large".to_string())?;
    let config = X2Config {
        pool: Pool::new(hosts, threads, mem_mib.unwrap_or(full.cap.mem_mib)),
        balance,
        slack_tol_ppm: tol.unwrap_or(base.slack_tol_ppm),
        key_share_ppm: share.unwrap_or(base.key_share_ppm),
        seed: seed.unwrap_or(base.seed),
        warm_up_min: base.warm_up_min,
    };
    if config.score().is_none() {
        return Err("--balance is above the scorer's range".to_string());
    }
    Ok(Args { export, config })
}
