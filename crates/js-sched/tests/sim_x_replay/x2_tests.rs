//! Tests of the x2 run: the truth of the real series, the learner's beliefs as the replay's,
//! the score rankers in the replay loop, the six-ranker report and the runner's flags. Each
//! test names the defect it would catch. None reads a clock or asserts a timing, and no
//! assertion reads a float.

use std::path::PathBuf;

use js_sched::{DecidedBy, MAX_BALANCE, ScoreConfig};

use crate::engine::adapter::{FLOOR_PERMILLE, ScoreModel, slot0_left};
use crate::engine::export::{Arrivals, Export, KeyProfile, Run};
use crate::engine::fixture::KeyIx;
use crate::engine::metrics::Decided;
use crate::engine::model::{Belief, Beliefs, COLD_CHARGE, Charge, HostState, Pool, Truth};
use crate::engine::predicate::{Params, left};
use crate::engine::predictor::Tail;
use crate::engine::rankers::{self, RankerKind, differing};
use crate::engine::rules::{Choose, Ranker};
use crate::engine::score_rule::{ScoreRule, host_name, score_host, score_lease};
use crate::engine::x2::{
    Args, BeliefTable, LABELS, SeriesTruth, X2Config, flatten, parse_args, replay_beliefs, run,
};

const MIN: u64 = 60_000;
const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;

/// A run of key `key` starting at `start_ms`, with `cores` per minute and `ws` bytes at every
/// minute, `wall_ms` long.
fn run_of(key: u16, start_ms: u64, wall_ms: u64, cores: &[u32], ws: u64) -> Run {
    assert_eq!(
        cores.len() as u64,
        wall_ms / MIN + 1,
        "a run's block has its minutes"
    );
    Run {
        key: KeyIx(key),
        platform: "any".to_string(),
        start_ms: Some(start_ms),
        wall_ms,
        cores_milli: cores.to_vec(),
        ws_bytes: vec![ws; cores.len()],
    }
}

fn export_of(keys: usize, runs: Vec<Run>) -> Export {
    Export {
        profile: KeyProfile {
            keys: (0..keys as u64).map(|k| 0xa0 + k).collect(),
            runs,
        },
        nodes: Vec::new(),
    }
}

/// Asserts each second of a run burns its own minute's mean cores, the last second only
/// its part of a second, nothing after the run, and that the seconds add up to the row's
/// CPU time exactly; and that the working set of a second is its minute's, rounded up to a
/// MiB, the last minute's held past the end. Catches a truth that spreads the run's mean
/// evenly (the shape lost), a last second charged whole, and a working set read from the
/// wrong minute.
#[test]
fn the_truth_burns_each_minute_of_the_real_series() {
    let mut run = run_of(0, 0, 150_500, &[1_000, 3_000, 500], GIB);
    run.ws_bytes = vec![MIB + 1, 2 * MIB, 3 * MIB];
    let export = export_of(1, vec![run]);
    let truth = SeriesTruth::new(&export);
    let ticks = 151;
    let burnt: Vec<u64> = (0..ticks + 3).map(|t| truth.core_ms(0, t)).collect();
    assert!(burnt[..60].iter().all(|&c| c == 1_000));
    assert!(burnt[60..120].iter().all(|&c| c == 3_000));
    assert!(burnt[120..150].iter().all(|&c| c == 500));
    assert_eq!(burnt[150], 250);
    assert!(burnt[151..].iter().all(|&c| c == 0));
    assert_eq!(burnt.iter().sum::<u64>(), export.rows()[0].cpu_ms);
    assert_eq!(
        [0, 59, 60, 120, 151, 10_000].map(|t| truth.working_set_mib(0, t)),
        [2, 2, 2, 3, 3, 3]
    );
}

/// Asserts the leases are the export's runs in start order. Catches a truth indexed by file
/// order, which replays one run's series under another's row.
#[test]
fn the_truth_is_in_start_order() {
    let export = export_of(
        2,
        vec![
            run_of(0, 5_000, 1_000, &[4_000], GIB),
            run_of(1, 0, 1_000, &[2_000], GIB),
        ],
    );
    let truth = SeriesTruth::new(&export);
    assert_eq!((truth.core_ms(0, 0), truth.core_ms(1, 0)), (2_000, 4_000));
}

/// Three runs of key 0 that end before a fourth starts: the fourth is warm and its belief is
/// the P90 curve of the three. Key 1's lease is cold.
fn learning_export() -> Export {
    let shapes: [&[u32]; 3] = [
        &[4_000, 1_000, 2_000],
        &[5_000, 1_000, 1_000],
        &[3_000, 2_000, 1_500],
    ];
    let mut runs: Vec<Run> = shapes
        .iter()
        .enumerate()
        .map(|(i, cores)| run_of(0, i as u64 * 200_000, 150_000, cores, GIB))
        .collect();
    runs.push(run_of(0, 700_000, 150_000, &[1, 1, 1], GIB));
    runs.push(run_of(1, 700_000, 1_000, &[1], GIB));
    export_of(2, runs)
}

/// Asserts the replay's belief of a warm lease is the learner's: the flag, `L`, `p_bar` and
/// the curve at every minute (the last point held past the end), and that a cold lease is
/// charged the cold charge at every slot. Catches a conversion that drops the curve (every
/// lease flat), shifts it by a minute, or loses the cold flag.
#[test]
fn the_replay_believes_what_the_learner_learnt() {
    let export = learning_export();
    let predictor = export.beliefs();
    let table = replay_beliefs(&predictor);
    assert_eq!(table.0.len(), 5);
    let learnt = predictor.belief(3);
    let b = table.belief(3);
    assert!(!learnt.is_cold() && !b.cold);
    assert_eq!(b.fit_minutes, learnt.life_minutes());
    assert_eq!(
        b.p_bar,
        Charge::new(learnt.p_bar_cores_milli(), learnt.p_bar_mem_mib())
    );
    for minute in 0..6 {
        assert_eq!(
            b.at(minute as u64),
            Charge::new(
                learnt.cores_milli_at(minute, Tail::Held),
                learnt.mem_mib_at(minute, Tail::Held)
            ),
            "minute {minute}"
        );
    }
    assert_ne!(b.at(0), b.at(1), "the curve has a shape");
    let cold = table.belief(4);
    assert!(cold.cold);
    assert!((0..40).all(|m| cold.at(m) == COLD_CHARGE));
}

/// Asserts flattening charges every belief its flat charge at every minute with `L = 1`, and
/// keeps the cold flag and `p_bar`. Catches FW5 fed curves (no flat side to compare with), or
/// a flat side that loses the cold cap.
#[test]
fn flattened_beliefs_have_no_shape() {
    let table = replay_beliefs(&learning_export().beliefs());
    let flat = flatten(&table);
    for (b, f) in table.0.iter().zip(&flat.0) {
        assert_eq!((f.cold, f.fit_minutes, f.p_bar), (b.cold, 1, b.p_bar));
        assert!((0..40).all(|m| f.at(m) == b.p_bar));
    }
}

/// A host of `Pool::ci` holding a resident of key 0 placed at tick 0 (covered by the round
/// at tick 5), one of key 1 and one of key 0 placed in the round at tick 5, read once.
fn busy_host<'w>(beliefs: &'w [Belief]) -> HostState<'w> {
    let mut host = HostState::new(Pool::ci().cap);
    host.begin_round(0);
    host.place(0, 0, &beliefs[0]);
    host.begin_round(5);
    host.place(1, 5, &beliefs[1]);
    host.place(2, 5, &beliefs[2]);
    host.read(5 * 3_500, 900);
    host
}

fn shaped(cores: &[u64], mem_mib: u64) -> Belief {
    let curve = cores.iter().map(|c| Charge::new(*c, mem_mib)).collect();
    Belief::new(
        false,
        cores.len() as u8,
        Charge::new(cores[0], mem_mib),
        curve,
    )
}

/// Asserts the scorer's view of a host is the replay's: slot 0 worked out from it equals the
/// predicate's left side (CPU, and memory in bytes), the later slots are the host's sums,
/// the same-key share counts only residents of the lease's key, and the uncovered leases are
/// those of the latest round. Catches a scorer that sees other figures than the predicate
/// (a curve charged in one and not the other), a same-key share over every resident, and
/// uncovered leases counted from the first round.
#[test]
fn the_scorer_sees_what_the_predicate_reads() {
    let beliefs = vec![
        shaped(&[2_000, 500, 500], 1_000),
        shaped(&[1_000, 3_000], 2_000),
        shaped(&[700, 700, 4_000], 500),
    ];
    let host = busy_host(&beliefs);
    let keys = [KeyIx(0), KeyIx(1), KeyIx(0), KeyIx(0)];
    let seen = score_host(&host, KeyIx(0), &keys);
    let params = Params::default();
    let predicate = left(&params, &host);
    let (slot0, _) = slot0_left(&seen.slot0, params.f_permille);
    assert_eq!(slot0.cpu_millis, predicate.cpu_millis);
    assert_eq!(slot0.memory_bytes, predicate.mem_mib * MIB);
    for slot in 1..crate::engine::model::SLOTS {
        let sum = host.sums().slots[slot];
        assert_eq!(
            seen.ahead[slot - 1].cpu_millis,
            sum.cpu_millis,
            "slot {slot}"
        );
        assert_eq!(
            seen.ahead[slot - 1].memory_bytes,
            sum.mem_mib * MIB,
            "slot {slot}"
        );
    }
    assert_eq!(seen.same_key_millis, 2_000 + 700);
    assert_eq!(seen.uncovered_leases, 2);
    assert_eq!(seen.cpu_capacity_millis, Pool::ci().cap.cpu_millis);
    assert_eq!(seen.memory_capacity_bytes, Pool::ci().cap.mem_mib * MIB);
    assert_eq!(FLOOR_PERMILLE, params.f_permille);
}

/// Asserts the score rule places where the score ranker it wraps places on the same model,
/// reports the rule that decided it, `64 x` the candidates as its evaluations and every
/// candidate as scored; and that a lone candidate is taken unscored. Catches a rule that
/// scores other hosts than the candidates, maps a host index to the wrong name, or counts
/// evaluations the loop bounds then misread.
#[test]
fn the_score_rule_places_where_the_scorer_places() {
    let beliefs = vec![
        shaped(&[2_000, 500, 500], 1_000),
        shaped(&[1_000, 3_000], 2_000),
        shaped(&[700, 700, 4_000], 500),
        shaped(&[3_000, 3_000, 200], 800),
    ];
    let cap = Pool::ci().cap;
    let hosts = vec![
        busy_host(&beliefs),
        HostState::new(cap),
        busy_host(&beliefs),
    ];
    let keys = vec![KeyIx(0), KeyIx(1), KeyIx(0), KeyIx(1)];
    let params = Params::default();
    let candidates = [0, 2, 1];
    let choose = Choose {
        lease: 3,
        tick: 10,
        belief: &beliefs[3],
        hosts: &hosts,
        candidates: &candidates,
        params: &params,
    };
    for kind in [RankerKind::Linear, RankerKind::Flat, RankerKind::Score] {
        let mut rule = ScoreRule::new(kind, ScoreConfig::default(), keys.clone());
        let pick = rule.choose(&choose);

        let mut direct = rankers::Ranker::new(kind, ScoreConfig::default());
        let model = ScoreModel::new(
            score_lease(&choose, &cap),
            params.f_permille,
            candidates
                .iter()
                .map(|&h| (host_name(h), score_host(&hosts[h], KeyIx(1), &keys))),
        );
        let names: Vec<_> = candidates.iter().map(|&h| host_name(h)).collect();
        let (chosen, by) = direct.place_explained(&model, &names).unwrap();
        assert_eq!(host_name(pick.host), *chosen, "{kind:?}");
        assert_ne!(by, DecidedBy::Only);
        assert_eq!(pick.decided, crate::engine::score_rule::decided(by));
        assert_eq!(pick.scored_candidates, 3);
        assert_eq!(pick.evaluations, direct.evaluations());
        if kind != RankerKind::Flat {
            assert_eq!(pick.evaluations, 3 * 64);
        }
        assert_eq!(rule.ranker().placements(), direct.placements());
    }

    let mut rule = ScoreRule::new(RankerKind::Score, ScoreConfig::default(), keys);
    let alone = rule.choose(&Choose {
        candidates: &[2],
        ..choose
    });
    assert_eq!(
        (
            alone.host,
            alone.decided,
            alone.scored_candidates,
            alone.evaluations
        ),
        (2, Decided::Only, 0, 0)
    );
}

/// Asserts each of the scorer's decisions has the replay's name.
#[test]
fn every_decision_has_its_replay_name() {
    use crate::engine::score_rule::decided;
    assert_eq!(decided(DecidedBy::Key(5)), Decided::Key(5));
    assert_eq!(decided(DecidedBy::PackLimit), Decided::PackLimit);
    assert_eq!(decided(DecidedBy::TieBreak(4)), Decided::TieBreak(4));
    assert_eq!(decided(DecidedBy::Only), Decided::Only);
}

/// A world of two keys of 10-minute runs, one arriving every 30 s, alternating; `shape`
/// gives key `k`'s cores at minute `m`.
fn world(shape: impl Fn(u16, usize) -> u32) -> Export {
    let runs = (0..160u64)
        .map(|i| {
            let key = (i % 2) as u16;
            let cores: Vec<u32> = (0..11).map(|m| shape(key, m)).collect();
            run_of(key, i * 30_000, 10 * MIN, &cores, 2 * GIB)
        })
        .collect();
    export_of(2, runs)
}

/// Key 0 is busy for its first five minutes and quiet after, key 1 the other way round.
fn shaped_world() -> Export {
    world(|key, m| match (key, m < 5) {
        (0, true) | (1, false) => 9_000,
        _ => 500,
    })
}

fn small() -> X2Config {
    X2Config {
        pool: Pool::new(3, 16, 64 * 1_024),
        warm_up_min: 0,
        ..X2Config::default()
    }
}

/// Asserts the report runs every ranker in order over the export's real series and starts:
/// the books of each close (every lease accounted for, each completed lease burnt its real
/// CPU time), the loop bounds hold (exactly `64 x` the candidates for every score ranker
/// but the flat fit), every score ranker scored placements, and the text names every
/// ranker, the gain lines and the guards. Catches a ranker left out or run on other
/// beliefs, a truth that does not close the books, and a score rule the loop miscounts.
#[test]
fn the_report_runs_every_ranker_on_the_real_series() {
    let export = shaped_world();
    assert_eq!(export.arrivals(), Arrivals::Recorded);
    let report = run(&export, &small());
    let labels: Vec<&str> = report.runs.iter().map(|r| r.label).collect();
    assert_eq!(labels, LABELS);
    for r in &report.runs {
        assert_eq!(r.books(), Ok(()), "{}", r.label);
        assert_eq!(r.loop_bounds(3), Ok(()), "{}", r.label);
        assert_eq!(r.replay.metrics.counters.submitted, 160, "{}", r.label);
        assert_eq!(
            r.score.is_some(),
            r.replay.metrics.counters.scored_placements > 0,
            "{}",
            r.label
        );
    }
    let text = report.render();
    for label in LABELS {
        assert!(text.contains(&format!("== {label} ==")), "{label}");
    }
    for line in [
        "real: arrivals (recorded starts)",
        "pool: 3 hosts of 16000 millicores, 65536 MiB, 64 leases (invented)",
        "score flags: balance 1 slack tol 10000 ppm key share 250000 ppm; random seed 1; warm-up 0 min",
        "gain 5 score over FW5 score on flat beliefs:",
        "gain 5 score over 4 flat (key 5 on window means):",
        "gain 5 score over 1 least-loaded:",
        "GD 5 score vs FW5 score on flat beliefs:",
        "GD 5 score vs 4 flat (key 5 on window means):",
    ] {
        assert!(text.contains(line), "{line}\n{text}");
    }
    assert_eq!(
        text,
        run(&export, &small()).render(),
        "the report is deterministic"
    );
}

/// Asserts the score with curves places differently from the same score on flat beliefs
/// when the keys' shapes complement each other (one key busy early, the other late).
/// Catches an x2 that cannot see shape: the curves lost on the way to the predicate or the
/// scorer, or FW5 fed the curves too, so that the gain line compares a ranker with itself.
#[test]
fn curves_change_placements_where_runs_have_a_shape() {
    let report = run(&shaped_world(), &small());
    let placements = |label: &str| {
        report
            .run(label)
            .score
            .as_ref()
            .expect("a score ranker")
            .ranker()
            .placements()
            .to_vec()
    };
    let (curved, flat) = (placements(LABELS[4]), placements(LABELS[5]));
    assert_eq!((curved.len(), flat.len()), (160, 160));
    assert!(
        differing(&curved, &flat) > 0,
        "the shapes moved no placement"
    );
}

/// Asserts the default run is the scorer's default flags on the full pool. Catches the
/// mirrored defaults drifting from the scorer's.
#[test]
fn the_default_run_is_the_scorers_defaults() {
    let d = X2Config::default();
    assert_eq!(d.score(), Some(ScoreConfig::default()));
    assert_eq!((d.pool, d.seed, d.warm_up_min), (Pool::full(), 1, 60));
}

/// The runner's argument handling: the shape accepted, each flag read, and each refusal.
/// Catches a flag read into another's place, a flag given twice that is taken, a pool of no
/// host or thread, and a balance the scorer refuses that reaches the run.
#[test]
fn the_runner_flags_are_export_once_and_each_setting_at_most_once() {
    let args = |v: &[&str]| parse_args(v.iter().map(|s| s.to_string()));
    let export = |v: &[&str]| args(v).map(|a| a.export);
    assert_eq!(
        args(&["--export", "d"]),
        Ok(Args {
            export: PathBuf::from("d"),
            config: X2Config::default(),
        })
    );
    assert_eq!(export(&[]), Err("--export is required".to_string()));
    assert_eq!(
        export(&["--export"]),
        Err("--export needs a directory".to_string())
    );
    assert_eq!(
        export(&["--export", "a", "--export", "b"]),
        Err("--export given twice".to_string())
    );
    assert_eq!(
        export(&["--bogus"]),
        Err("unknown argument \"--bogus\"".to_string())
    );
    assert_eq!(
        export(&["--export", "d", "extra"]),
        Err("unknown argument \"extra\"".to_string())
    );

    let all = args(&[
        "--export",
        "d",
        "--hosts",
        "4",
        "--threads",
        "8",
        "--mem-mib",
        "1024",
        "--balance",
        "2",
        "--slack-tol-ppm",
        "5",
        "--key-share-ppm",
        "6",
        "--seed",
        "7",
    ])
    .unwrap();
    assert_eq!(
        all.config,
        X2Config {
            pool: Pool::new(4, 8, 1_024),
            balance: 2,
            slack_tol_ppm: 5,
            key_share_ppm: 6,
            seed: 7,
            warm_up_min: 60,
        }
    );
    let refused = |v: &[&str]| args(v).unwrap_err();
    assert_eq!(
        refused(&["--export", "d", "--seed"]),
        "--seed needs a number"
    );
    assert_eq!(
        refused(&["--export", "d", "--seed", "x"]),
        "--seed is not a whole number: \"x\""
    );
    assert_eq!(
        refused(&["--export", "d", "--seed", "1", "--seed", "2"]),
        "--seed given twice"
    );
    assert_eq!(
        refused(&["--export", "d", "--hosts", "0"]),
        "--hosts must be at least 1"
    );
    assert_eq!(
        refused(&["--export", "d", "--threads", "0"]),
        "--threads must be 1 to 1073741823"
    );
    assert_eq!(
        refused(&["--export", "d", "--threads", "1073741824"]),
        "--threads must be 1 to 1073741823"
    );
    let over = (MAX_BALANCE + 1).to_string();
    assert_eq!(
        refused(&["--export", "d", "--balance", &over]),
        "--balance is above the scorer's range"
    );
    assert_eq!(
        refused(&["--export", "d", "--balance", "4294967296"]),
        "--balance is too large"
    );
}

/// The beliefs the replay reads are a table, one per lease.
#[test]
fn a_belief_table_answers_by_lease() {
    let t = BeliefTable(vec![Belief::cold(), Belief::flat(Charge::new(1, 2))]);
    assert!(t.belief(0).cold);
    assert_eq!(t.belief(1).p_bar, Charge::new(1, 2));
}
