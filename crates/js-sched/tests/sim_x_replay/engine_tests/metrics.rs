//! The integer metrics, the guards that fail short or overloaded runs, and the engine's
//! counted bounds.

use super::{TableBeliefs, busy, flat, row, run, small_config};
use crate::engine::metrics::Metrics;
use crate::engine::metrics::{
    Counters, Fnv, Guards, LoopBounds, Unbalanced, Uninformative, cpu_ppm, long_run_share_ppm,
    nearest_rank, oversize_rows,
};
use crate::engine::model::Pool;
use crate::engine::rankers::{Choose, LeastLoaded, Pick, Ranker};
use crate::engine::replay::Config;

/// Catches: a percentile off by one rank, taken by interpolation, or over an empty or
/// one-element set: nearest rank is the `ceil(p n)`-th smallest.
#[test]
fn percentiles_are_nearest_rank() {
    let ten: Vec<u64> = (1..=10).collect();
    assert_eq!(nearest_rank(&ten, 50, 100), Some(5));
    assert_eq!(nearest_rank(&ten, 51, 100), Some(6));
    assert_eq!(nearest_rank(&ten, 90, 100), Some(9));
    assert_eq!(nearest_rank(&ten, 99, 100), Some(10));
    assert_eq!(
        nearest_rank(&ten, 0, 100),
        Some(1),
        "rank 0 is the smallest"
    );
    assert_eq!(nearest_rank(&ten, 100, 100), Some(10));
    let thousand: Vec<u64> = (1..=1_000).collect();
    assert_eq!(nearest_rank(&thousand, 99, 100), Some(990));
    assert_eq!(nearest_rank(&thousand, 999, 1000), Some(999));
    assert_eq!(nearest_rank(&[7], 99, 100), Some(7));
    assert_eq!(nearest_rank(&[], 50, 100), None);
}

/// Catches: a share computed in floats or against the wrong capacity: a window's ppm is
/// its core-ms over `C_cpu` times its seconds.
#[test]
fn shares_are_exact_integers() {
    // 8 cores for 5 s is 40 000 core-ms: a full window of an 8-core host.
    assert_eq!(cpu_ppm(40_000, 8_000, 5), 1_000_000);
    assert_eq!(cpu_ppm(10_000, 8_000, 5), 250_000);
    assert_eq!(cpu_ppm(1, 8_000, 5), 25, "floor, not rounded");
    assert!(cpu_ppm(u64::MAX, 8_000, 5) > 0, "no overflow");
    assert_eq!(cpu_ppm(5, 0, 5), 0, "a host with no cores");
    let mut a = Fnv::new();
    a.u64(1);
    a.u64(2);
    let mut b = Fnv::new();
    b.u64(2);
    b.u64(1);
    assert_ne!(a, b, "the hash depends on order");
}

/// Catches: the long-run share and the oversize count reading the wrong field or the
/// wrong threshold: 0.9 of the host is not oversize, one more milli of a core is.
#[test]
fn oversize_and_long_run_counts() {
    let cap = Pool::new(1, 10, 1_000).cap; // 10 cores, 1000 MiB
    let rows = [
        row(0, 1_000, 9_000, 100),  // 9.0 cores: at 0.9, not over
        row(0, 1_000, 9_001, 100),  // over on cores
        row(0, 1_000, 1_000, 900),  // 900 MiB: at 0.9, not over
        row(0, 1_000, 1_000, 901),  // over on memory
        row(0, 1_000, 20_000, 901), // over on both: counted once
    ];
    assert_eq!(oversize_rows(&rows, &cap), 3);
    let rows = [
        row(0, 300_000, 1, 1),
        row(0, 300_001, 1, 1),
        row(0, 100_000, 1, 1),
    ];
    assert_eq!(long_run_share_ppm(&rows), 300_001 * 1_000_000 / 700_001);
}

/// Catches: a guard that cannot fail. The busy world run for 6 hours passes the guards
/// with the sample counts scaled to the pool; the 20-row trace of a short test fails
/// every one of the sample guards, and the default (registered) limits fail the busy
/// world too, because it is two small hosts and not the pool they are written for.
#[test]
fn sample_guards_fail_a_short_trace() {
    let w = busy();
    let scaled = Guards {
        min_window_samples: 5_000,
        min_minute_samples: 600,
        ..Guards::default()
    };
    assert_eq!(
        scaled.check(&w.run.metrics),
        Ok(()),
        "{}",
        w.run.metrics.render()
    );

    let rows: Vec<_> = (0..20).map(|i| row(i * 3_000, 5_000, 7_500, 512)).collect();
    let beliefs = TableBeliefs::uniform(rows.len(), &flat(2_000, 600));
    let mut cfg = small_config(2, 4, 16_384);
    cfg.warm_up_min = 0;
    let short = run(&cfg, &rows, &beliefs, &mut LeastLoaded);
    let bad = Guards::default()
        .check(&short.metrics)
        .expect_err("a minute of data");
    assert!(
        bad.iter()
            .any(|u| matches!(u, Uninformative::WindowSamples { .. }))
    );
    assert!(
        bad.iter()
            .any(|u| matches!(u, Uninformative::MinuteSamples { .. }))
    );
    assert!(bad.iter().any(|u| matches!(u, Uninformative::Span { .. })));
    assert!(
        bad.iter()
            .any(|u| matches!(u, Uninformative::ColdShare { .. }))
    );
    assert!(bad[0].to_string().starts_with("UNINFORMATIVE: "));
}

/// Catches: the cold-share and oversize guards reading the wrong share: a world with no
/// cold lease is uninformative, and so is one whose rows do not fit a host.
#[test]
fn cold_and_oversize_guards() {
    let w = busy();
    let relaxed = Guards {
        min_window_samples: 0,
        min_minute_samples: 0,
        min_span_minutes: 0,
        ..Guards::default()
    };
    assert_eq!(relaxed.check(&w.run.metrics), Ok(()));
    let all_warm = TableBeliefs::uniform(w.rows.len(), &flat(2_000, 600));
    let r = run(&w.cfg, &w.rows, &all_warm, &mut LeastLoaded);
    assert_eq!(
        relaxed.check(&r.metrics),
        Err(vec![Uninformative::ColdShare { ppm: 0 }])
    );
    let big: Vec<_> = (0..w.rows.len() as u64)
        .map(|i| row(i * 3_000, 4_000, 4_000 * 40, 512))
        .collect();
    let r = run(&w.cfg, &big, &w.beliefs, &mut LeastLoaded);
    let bad = relaxed.check(&r.metrics).expect_err("rows of 40 cores");
    assert!(
        bad.iter()
            .any(|u| matches!(u, Uninformative::Oversize { .. }))
    );
}

/// Catches: a stability guard that lets a pool that never drains through: with every host
/// marked, nothing is placed, the queue is the whole trace at the end and its waits grow
/// with the run. The guard names both, and the books still balance.
#[test]
fn the_stability_guard_trips_on_a_queue_that_never_drains() {
    let w = busy();
    let mut cfg = w.cfg.clone();
    cfg.excluded = vec![0, 1];
    cfg.drain_ticks = 600;
    let r = run(&cfg, &w.rows, &w.beliefs, &mut LeastLoaded);
    let c = &r.metrics.counters;
    assert_eq!((c.placed, c.completed), (0, 0));
    assert_eq!(c.queued_at_end, w.rows.len() as u64);
    assert_eq!(r.metrics.check_books(), Ok(()));
    let relaxed = Guards {
        min_window_samples: 0,
        min_minute_samples: 0,
        min_span_minutes: 0,
        cold_share_ppm: (0, 1_000_000),
        ..Guards::default()
    };
    let bad = relaxed.check(&r.metrics).expect_err("nothing was served");
    assert!(
        bad.iter()
            .any(|u| matches!(u, Uninformative::QueuedAtEnd { .. }))
    );
    assert!(
        bad.iter()
            .any(|u| matches!(u, Uninformative::WaitP99 { .. }))
    );
    // The same pool unmarked is fine: the guard fires on the queue, not on the world.
    assert_eq!(relaxed.check(&w.run.metrics), Ok(()));
}

/// A world of two 4-thread hosts and a long lease that believes it needs all of a host
/// while burning an eighth of it. With a second such lease and a waiter at 10 s the waiter
/// is refused on both hosts for 20 minutes behind the prediction: the capacity is idle and
/// the predicate says it is full. Without them nothing is refused.
fn stranded_world(with_waiter: bool) -> (Config, Vec<crate::engine::fixture::Row>, TableBeliefs) {
    let cfg = small_config(2, 4, 16_384);
    let mut rows = vec![row(0, 1_200_000, 600_000, 100)];
    let mut beliefs = vec![flat(4_000, 100)];
    if with_waiter {
        rows.push(row(0, 1_200_000, 600_000, 100));
        beliefs.push(flat(4_000, 100));
        rows.push(row(10_000, 60_000, 60_000, 100));
        beliefs.push(flat(2_000, 100));
    }
    // The last arrival closes the metric window: ten whole minutes.
    rows.push(row(600_000, 1_000, 100, 100));
    beliefs.push(flat(100, 100));
    (cfg, rows, TableBeliefs(beliefs))
}

/// Catches: stranding (a) that is zero by construction, or that is just idle capacity.
/// The hand world has capacity that is idle (an eighth used) and a lease the predicate
/// refuses on both hosts: every host-minute has a refusal and `87.5%` of the capacity
/// idle, so stranded (a) is 875 000 ppm. Without the waiter nothing is refused, the same
/// idle capacity is headroom (b) and stranded (a) is 0.
///
/// It also shows why the retired definition (idle on hosts that rejected nothing while
/// the queue stayed non-empty) is empty under a FIFO scan: a queue that is still
/// non-empty after a round means the head was refused by every host.
#[test]
fn stranding_is_idle_capacity_behind_a_refusal_and_the_retired_definition_is_empty() {
    let (cfg, rows, beliefs) = stranded_world(true);
    let r = run(&cfg, &rows, &beliefs, &mut LeastLoaded);
    let m = &r.metrics;
    assert_eq!(m.metric_minutes, 10);
    assert_eq!(m.host_minutes, 20);
    assert_eq!(m.refusal_host_minutes, 20);
    assert_eq!(m.stranded_a_rate_ppm(), 875_000);
    assert_eq!(m.headroom_b_rate_ppm(), 0);
    assert!(m.stranded_a_rate_ppm() > 0);
    assert_eq!(
        m.retired_host_minutes, 0,
        "the retired definition is the empty set"
    );
    assert_eq!(m.check_books(), Ok(()));

    let (cfg, rows, beliefs) = stranded_world(false);
    let r = run(&cfg, &rows, &beliefs, &mut LeastLoaded);
    let m = &r.metrics;
    assert_eq!(m.stranded_a_rate_ppm(), 0);
    // One host at an eighth, one empty: the idle capacity is the same kind, no refusal.
    assert_eq!(
        m.headroom_b_rate_ppm(),
        937_500,
        "idle capacity with no refusal behind it"
    );
    assert_eq!(m.retired_host_minutes, 0);
}

/// Catches: kept-empty hosts counted where consolidation was not possible, or empty
/// minutes counted wrongly. Two hosts, one lease on one of them: the pool's truth CPU
/// (an eighth of a host) fits on one host at 0.6, so the minutes are consolidation
/// minutes and the other host is empty in each. With the lease believed to need a whole
/// host and the second lease too, both are occupied and nothing is kept empty.
#[test]
fn kept_empty_hosts_count_only_where_consolidation_is_possible() {
    let mut cfg = small_config(2, 4, 16_384);
    cfg.warm_up_min = 0;
    let rows = vec![row(0, 600_000, 300_000, 100), row(600_000, 1_000, 100, 100)];
    let beliefs = TableBeliefs(vec![flat(1_000, 100), flat(100, 100)]);
    let r = run(&cfg, &rows, &beliefs, &mut LeastLoaded);
    let m = &r.metrics;
    assert_eq!(m.metric_minutes, 10);
    assert_eq!(m.consolidation_minutes, 10);
    assert_eq!(
        m.empty_host_minutes, 10,
        "host 1 is empty in all ten minutes"
    );
    assert_eq!(m.kept_empty_host_minutes, 10);

    // A pool as busy as 7 cores of 8 is over 0.6 (n - 1) C = 2.4 cores: not possible.
    let rows = vec![
        row(0, 600_000, 2_100_000, 100),
        row(0, 600_000, 2_100_000, 100),
        row(600_000, 1_000, 100, 100),
    ];
    let beliefs = TableBeliefs(vec![flat(3_500, 100), flat(3_500, 100), flat(100, 100)]);
    let r = run(&cfg, &rows, &beliefs, &mut LeastLoaded);
    let m = &r.metrics;
    assert_eq!(m.consolidation_minutes, 0);
    assert_eq!(m.empty_host_minutes, 0);
    assert_eq!(m.kept_empty_host_minutes, 0);
}

/// Catches: the leases-never-examined line missing, or counted per lease instead of per
/// round. 41 leases wait behind one that fills the host: each round looks at 32 of them
/// and leaves 9, the one that would fit among them. The table prints the numbers.
#[test]
fn rounds_that_stop_early_count_the_leases_they_never_looked_at() {
    let mut cfg = small_config(1, 4, 16_384);
    cfg.warm_up_min = 0;
    let mut rows = vec![row(0, 20_000, 20_000, 100)];
    let mut beliefs = vec![flat(4_000, 100)];
    for _ in 0..40 {
        rows.push(row(0, 1_000, 1_000, 100));
        beliefs.push(flat(1_000, 100));
    }
    rows.push(row(0, 1_000, 100, 100));
    beliefs.push(flat(100, 100));
    let r = run(&cfg, &rows, &TableBeliefs(beliefs), &mut LeastLoaded);
    let c = &r.metrics.counters;
    // Round 0: the first is placed, then 32 refusals, then 9 never looked at.
    assert_eq!(c.unexamined_max, 9);
    assert_eq!(
        c.worst_round_excess, 32,
        "attempts = placed + 32 at the most"
    );
    assert!(c.rounds_with_unexamined >= 1);
    assert_eq!(c.check_loop_bounds(1, true), Ok(()));
    let table = r.metrics.render();
    assert!(table.contains("never examined"), "{table}");
    assert!(table.contains("at most 9"), "{table}");
    // The small lease behind the 40 waits for a round that reaches it: it is placed
    // after the big one ends, not in the first round.
    let small = r.placements.iter().find(|p| p.lease == 41).expect("placed");
    assert!(
        small.tick >= 20,
        "it was not looked at while the host was full"
    );
    assert_eq!(r.metrics.check_books(), Ok(()));
}

/// Catches: the LoopBounds bounds that cannot trip. Each is broken in a counter set by hand (a
/// runaway that looks at a lease twice in a round, a round that looks past its refusals,
/// a scorer that reports evaluations that are not 64 per candidate or scored a host that
/// was not handed to it), and the clean counters of a real run pass.
#[test]
fn the_loop_bounds_trip_on_synthetic_loops() {
    let w = busy();
    assert_eq!(w.run.metrics.counters.check_loop_bounds(2, true), Ok(()));

    let base = Counters {
        attempts: 10,
        lease_rounds: 10,
        scored_candidates: 4,
        evaluations: 4 * 64,
        ..Counters::default()
    };
    assert_eq!(base.check_loop_bounds(2, true), Ok(()));
    let looped = Counters {
        attempts: 11,
        ..base.clone()
    };
    assert_eq!(
        looped.check_loop_bounds(2, true),
        Err(LoopBounds::Attempts {
            attempts: 11,
            bound: 10
        })
    );
    let past_refusals = Counters {
        worst_round_excess: 33,
        ..base.clone()
    };
    assert_eq!(
        past_refusals.check_loop_bounds(2, true),
        Err(LoopBounds::RoundAttempts { excess: 33 })
    );
    let over = Counters {
        scored_candidates: 21,
        evaluations: 21 * 64,
        ..base.clone()
    };
    assert_eq!(
        over.check_loop_bounds(2, true),
        Err(LoopBounds::ScoredOverHosts {
            scored: 21,
            bound: 20
        })
    );
    let twice = Counters {
        evaluations: 4 * 64 * 2,
        ..base.clone()
    };
    assert_eq!(
        twice.check_loop_bounds(2, true),
        Err(LoopBounds::Evaluations {
            evaluations: 512,
            expected: 256
        })
    );
    assert_eq!(
        twice.check_loop_bounds(2, false),
        Ok(()),
        "flat fit may evaluate more"
    );
    let fewer = Counters {
        evaluations: 4 * 64 - 1,
        ..base.clone()
    };
    assert!(fewer.check_loop_bounds(2, false).is_err());
    let beyond = Counters {
        scored_beyond_candidates: 1,
        ..base
    };
    assert_eq!(
        beyond.check_loop_bounds(2, true),
        Err(LoopBounds::ScoredBeyondCandidates { extra: 1 })
    );
}

/// A ranker that claims to have scored every host of the pool, candidates or not.
struct ScoresEverything;

impl Ranker for ScoresEverything {
    fn choose(&mut self, c: &Choose<'_, '_>) -> Pick {
        Pick {
            host: c.candidates[0],
            decided: crate::engine::metrics::Decided::Key(5),
            scored_candidates: c.hosts.len() as u64,
            evaluations: 64 * c.hosts.len() as u64,
        }
    }
}

/// Catches: a scorer evaluated for a host the predicate refused or a marked host. One
/// host is marked, so a scorer that scores the whole pool reports a candidate too many
/// on every placement, and the engine's count shows it.
#[test]
fn scoring_a_marked_or_refused_host_is_counted() {
    let w = busy();
    let mut cfg = w.cfg.clone();
    cfg.excluded = vec![1];
    let r = run(&cfg, &w.rows[..400], &w.beliefs, &mut ScoresEverything);
    let c = &r.metrics.counters;
    assert!(c.placed > 0);
    assert_eq!(
        c.scored_beyond_candidates, c.placed,
        "one host too many each time"
    );
    assert!(matches!(
        c.check_loop_bounds(2, true),
        Err(LoopBounds::ScoredBeyondCandidates { .. })
    ));
    assert!(
        r.placements.iter().all(|p| p.host == 0),
        "the marked host got nothing"
    );
    // Honest: a scorer that scores the candidates it is handed passes.
    let r = run(&w.cfg, &w.rows[..400], &w.beliefs, &mut Honest);
    let c = &r.metrics.counters;
    assert!(
        c.scored_placements > 0,
        "some placements had two candidates"
    );
    assert_eq!(c.check_loop_bounds(2, true), Ok(()));
    assert_eq!(
        r.metrics
            .decided
            .get(&crate::engine::metrics::Decided::Key(5))
            .copied()
            .unwrap_or(0),
        c.placed
    );
}

/// A scorer that scores exactly the candidates it is handed (when there are two or more).
struct Honest;

impl Ranker for Honest {
    fn choose(&mut self, c: &Choose<'_, '_>) -> Pick {
        let n = c.candidates.len() as u64;
        Pick {
            host: c.candidates[0],
            decided: crate::engine::metrics::Decided::Key(5),
            scored_candidates: if n >= 2 { n } else { 0 },
            evaluations: if n >= 2 { 64 * n } else { 0 },
        }
    }
}

/// Catches: books that balance by construction. A truth that burns one core-ms more than
/// the row says (a shape or burst that adds CPU) breaks the completed-lease identity; an
/// honest one balances.
#[test]
fn the_books_catch_a_truth_that_adds_cpu() {
    let w = busy();
    let rows = &w.rows[..200];
    let honest = super::EvenTruth::new(rows);
    let cfg = w.cfg.clone();
    let ok = crate::engine::replay::replay(&cfg, rows, &honest, &w.beliefs, &mut LeastLoaded);
    assert_eq!(ok.metrics.check_books(), Ok(()));
    let mut liar = super::EvenTruth::new(rows);
    liar.extra_first_tick = 1;
    let bad = crate::engine::replay::replay(&cfg, rows, &liar, &w.beliefs, &mut LeastLoaded);
    assert!(matches!(
        bad.metrics.check_books(),
        Err(Unbalanced::Completed { mismatched }) if mismatched == bad.metrics.counters.completed
    ));
}

/// Catches: window percentiles reported from too few samples. With 19 999 host-windows
/// the median, the 99th and the 99.9th are unreported; with 20 000 they are the nearest
/// ranks (the 10 000th, 19 800th and 19 980th smallest).
#[test]
fn window_percentiles_need_20_000_samples() {
    let below = Metrics {
        window_ppm: (1..=19_999).collect(),
        ..Metrics::default()
    };
    assert_eq!(below.window_p50(), None);
    assert_eq!(below.window_p99(), None);
    assert_eq!(below.window_p999(), None);
    assert!(below.render().contains("p50 - p99 - p99.9 -"));

    let at = Metrics {
        window_ppm: (1..=20_000).collect(),
        ..Metrics::default()
    };
    assert_eq!(at.window_p50(), Some(10_000));
    assert_eq!(at.window_p99(), Some(19_800));
    assert_eq!(at.window_p999(), Some(19_980));
}

/// Catches: the stability guard's edge: the queue must be under 1% of the submitted
/// leases at the end. Exactly 1% (10 000 ppm) is uninformative; one lease fewer is not.
#[test]
fn the_queued_at_end_guard_is_strictly_under_one_percent() {
    let relaxed = Guards {
        min_window_samples: 0,
        min_minute_samples: 0,
        min_span_minutes: 0,
        cold_share_ppm: (0, 1_000_000),
        ..Guards::default()
    };
    let with = |submitted: u64, queued_at_end: u64| Metrics {
        counters: Counters {
            submitted,
            queued_at_end,
            ..Counters::default()
        },
        ..Metrics::default()
    };
    assert_eq!(
        relaxed.check(&with(100, 1)),
        Err(vec![Uninformative::QueuedAtEnd {
            queued: 1,
            ppm: 10_000
        }])
    );
    assert_eq!(relaxed.check(&with(101, 1)), Ok(()), "9 900 ppm");
    assert_eq!(relaxed.check(&with(100, 0)), Ok(()));
}

/// A scorer that reports it scored one candidate.
struct ScoresOne;

impl Ranker for ScoresOne {
    fn choose(&mut self, c: &Choose<'_, '_>) -> Pick {
        Pick {
            host: c.candidates[0],
            decided: crate::engine::metrics::Decided::Only,
            scored_candidates: 1,
            evaluations: 64,
        }
    }
}

/// Catches: a placement counted as scored when a scorer ran on one candidate. A choice
/// between two hosts or more is a scored placement; the only host that passed is not.
#[test]
fn a_placement_is_scored_from_two_candidates() {
    let w = busy();
    let r = run(&w.cfg, &w.rows[..100], &w.beliefs, &mut ScoresOne);
    assert!(r.metrics.counters.placed > 0);
    assert_eq!(r.metrics.counters.scored_placements, 0);
}
