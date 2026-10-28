//! The loop on hand traces: kills and the books, what a run consumed, determinism, and a
//! slice of the committed trace.

use std::sync::OnceLock;

use super::{EvenTruth, TableBeliefs, busy, embedded, flat, row, run, small_config};
use crate::engine::fixture::Row;
use crate::engine::model::Belief;
use crate::engine::rankers::{LeastLoaded, Random};
use crate::engine::replay::{Replay, replay};

/// One host of 4 threads and 1000 MiB, and three leases of 100 s at 1 core.
fn kill_world(mem: [u64; 3], starts_ms: [u64; 3]) -> (Vec<Row>, TableBeliefs) {
    let rows: Vec<Row> = (0..3)
        .map(|i| row(starts_ms[i], 100_000, 100_000, mem[i]))
        .collect();
    (rows, TableBeliefs::uniform(3, &flat(500, 10)))
}

/// Catches: a kill that takes the wrong lease, that is requeued or double counted, or
/// books that lose the killed lease's CPU. Working sets 300, 450, 450 on a host of 1000
/// MiB exceed it at the first window's end; the largest goes, and between equals the
/// newest (the higher lease index, started in the same round). Then 400, 450 and a third
/// that starts later at 300: the largest, though not the newest, goes. The killed lease
/// burnt the five seconds it ran, never comes back, and the books balance exactly.
#[test]
fn memory_kills_take_the_largest_then_the_newest_and_the_books_balance() {
    let cfg = small_config(1, 4, 1_000);

    let (rows, beliefs) = kill_world([300, 450, 450], [0, 0, 0]);
    let r = run(&cfg, &rows, &beliefs, &mut LeastLoaded);
    assert_eq!(
        r.kills,
        vec![(2, 4)],
        "the newest of the two largest, at the window's end"
    );
    let m = &r.metrics;
    assert_eq!((m.counters.completed, m.counters.killed), (2, 1));
    assert_eq!(m.books.killed_core_ms, 5_000, "five ticks of 1000 core-ms");
    assert_eq!(m.books.completed_core_ms, 200_000);
    assert_eq!(m.books.host_core_ms, 205_000);
    assert_eq!(m.check_books(), Ok(()));
    assert_eq!(m.counters.check_a9(1, true), Ok(()));

    let (rows, beliefs) = kill_world([400, 450, 300], [0, 0, 5_000]);
    let r = run(&cfg, &rows, &beliefs, &mut LeastLoaded);
    assert_eq!(r.kills, vec![(1, 9)], "the largest, though not the newest");
    assert_eq!(r.metrics.counters.completed, 2);
    assert_eq!(r.metrics.check_books(), Ok(()));
    assert_eq!(r.metrics.books.killed_core_ms, 10_000);

    // Under the memory: nobody is killed.
    let (rows, beliefs) = kill_world([300, 300, 300], [0, 0, 0]);
    let r = run(&cfg, &rows, &beliefs, &mut LeastLoaded);
    assert!(r.kills.is_empty());
    assert_eq!(r.metrics.counters.completed, 3);
}

/// Catches: leases that vanish when a run stops early. A lease still running when the
/// drain ends is counted as running with the core-ms it burnt, and the identity holds.
#[test]
fn leases_still_running_at_the_end_are_accounted() {
    let mut cfg = small_config(1, 4, 16_384);
    cfg.drain_ticks = 10;
    let rows = vec![row(0, 100_000, 100_000, 100)];
    let beliefs = TableBeliefs::uniform(1, &flat(500, 10));
    let r = run(&cfg, &rows, &beliefs, &mut LeastLoaded);
    let c = &r.metrics.counters;
    assert_eq!(
        (c.completed, c.killed, c.queued_at_end, c.running_at_end),
        (0, 0, 0, 1)
    );
    assert_eq!(r.metrics.books.running_core_ms, 11_000, "ticks 0 to 10");
    assert_eq!(r.metrics.check_books(), Ok(()));
}

/// Catches: an arrival hash that depends on the ranker or the waits (it must be the same
/// for every ranker, so two runs are known to have consumed the same input), or that
/// ignores a row that moved: it is equal between the previous rule and the random one
/// while their placements differ, changes when a row arrives a round later, and not when
/// it moves within the same round.
#[test]
fn every_ranker_consumes_the_same_arrivals() {
    let w = busy();
    let rows = &w.rows[..500];
    let ll = run(&w.cfg, rows, &w.beliefs, &mut LeastLoaded);
    let rnd = run(&w.cfg, rows, &w.beliefs, &mut Random::new(1));
    assert_ne!(
        ll.placement_hash, rnd.placement_hash,
        "the rankers decide differently"
    );
    assert_ne!(ll.placements, rnd.placements);
    assert_eq!(ll.arrival_hash, rnd.arrival_hash);

    let mut later = rows.to_vec();
    let last = later.len() - 1;
    later[last].start_ms += 5_000;
    let moved = run(&w.cfg, &later, &w.beliefs, &mut LeastLoaded);
    assert_ne!(moved.arrival_hash, ll.arrival_hash);

    // Row 499 starts at 1 497 000 ms; round ticks are 5 s apart, so +1 ms is the same round.
    let mut within = rows.to_vec();
    within[last].start_ms += 1;
    let same = run(&w.cfg, &within, &w.beliefs, &mut LeastLoaded);
    assert_eq!(same.arrival_hash, ll.arrival_hash);
}

/// Catches: anything that makes two runs of one input differ (an ordered map swapped for
/// a hashed one, a clock, an address): the placements, their hash and the printed table
/// are the same on a second run.
#[test]
fn a_run_is_deterministic() {
    let w = busy();
    let again = run(&w.cfg, &w.rows, &w.beliefs, &mut LeastLoaded);
    assert_eq!(again.placement_hash, w.run.placement_hash);
    assert_eq!(again.arrival_hash, w.run.arrival_hash);
    assert_eq!(again.metrics.render(), w.run.metrics.render());
    assert_eq!(again.metrics.window_ppm, w.run.metrics.window_ppm);
    assert_eq!(again.metrics.counters, w.run.metrics.counters);
}

/// Catches: the busy world not being a test of anything: it places everything, books
/// balance, cold leases are counted per hour, and the bounds hold.
#[test]
fn the_busy_world_balances() {
    let w = busy();
    let m = &w.run.metrics;
    let c = &m.counters;
    assert_eq!(c.submitted, 7_200);
    assert_eq!(c.placed, c.submitted);
    assert_eq!(c.completed, c.submitted);
    assert_eq!(m.check_books(), Ok(()));
    assert_eq!(c.check_a9(2, true), Ok(()));
    let placed: u64 = m.by_hour.values().map(|h| h.placed).sum();
    let cold: u64 = m.by_hour.values().map(|h| h.cold).sum();
    assert_eq!((placed, cold), (7_200, 720));
    assert_eq!(m.by_hour.len(), 6);
    assert!(m.completed_per_1000_min() > 0);
    // Hosts are used in turn by the previous rule: both carry work.
    let on_second = w.run.placements.iter().filter(|p| p.host == 1).count();
    assert!(on_second > 1_000, "{on_second}");
}

/// Catches: the previous rule through the engine ranking by CPU alone. A host at 61% of
/// its memory and 7.5% of its cores is more loaded than one at 37.5% of its cores and
/// none of its memory, so the third lease goes to the second host; a rule that read only
/// the cores would send it to the first.
#[test]
fn least_loaded_ranks_by_the_larger_share() {
    let cfg = small_config(2, 4, 16_384);
    let rows = vec![
        row(0, 600_000, 60_000, 10_000),
        row(0, 600_000, 60_000, 100),
        row(10_000, 600_000, 60_000, 100),
    ];
    let beliefs = TableBeliefs(vec![flat(400, 10_000), flat(2_000, 100), flat(100, 100)]);
    let r = run(&cfg, &rows, &beliefs, &mut LeastLoaded);
    let hosts: Vec<(usize, usize)> = r.placements.iter().map(|p| (p.lease, p.host)).collect();
    assert_eq!(hosts, vec![(0, 0), (1, 1), (2, 1)]);
}

/// Catches: a marked host that is placed on, or whose absence changes who else is
/// considered: with host 0 marked every lease runs on host 1.
#[test]
fn a_marked_host_is_never_placed_on() {
    let w = busy();
    let mut cfg = w.cfg.clone();
    cfg.excluded = vec![0];
    let r = run(&cfg, &w.rows[..300], &w.beliefs, &mut LeastLoaded);
    assert!(!r.placements.is_empty());
    assert!(r.placements.iter().all(|p| p.host == 1));
}

/// Catches: a resident charged at its starting age for good. A lease believed to be at
/// 1 core for two minutes, then 4 cores for one, then 1 again refuses a second lease of 2
/// cores while the rise is within the horizon (age 0 and 1: slot 2, then slot 1), and
/// admits it in the round at which the rise has passed, 120 s on (age 2). Without ageing
/// the second would wait until the first ends at 300 s.
#[test]
fn a_resident_is_re_charged_as_it_ages() {
    use crate::engine::model::Charge;
    let cfg = small_config(1, 4, 16_384);
    let low = Charge::new(1_000, 10);
    let high = Charge::new(4_000, 10);
    let rising = Belief::new(false, 3, low, vec![low, low, high, low]);
    let rows = vec![
        row(0, 300_000, 300_000, 10),
        row(60_000, 10_000, 10_000, 10),
    ];
    let beliefs = TableBeliefs(vec![rising, flat(2_000, 10)]);
    let r = run(&cfg, &rows, &beliefs, &mut LeastLoaded);
    let second = r.placements.iter().find(|p| p.lease == 1).expect("placed");
    assert_eq!(second.tick, 120);
    assert_eq!(r.metrics.check_books(), Ok(()));
}

/// The first 3000 rows of the committed trace, with beliefs that are the row's own cores
/// and memory once its key has run three times before in the slice, and cold before.
fn smoke() -> &'static (Vec<Row>, TableBeliefs, Replay) {
    static S: OnceLock<(Vec<Row>, TableBeliefs, Replay)> = OnceLock::new();
    S.get_or_init(|| {
        let f = embedded();
        let rows: Vec<Row> = f.rows[..3_000].to_vec();
        let mut seen = std::collections::BTreeMap::new();
        let beliefs: Vec<Belief> = rows
            .iter()
            .map(|r| {
                let n = seen.entry(r.key).or_insert(0u32);
                *n += 1;
                if *n <= 3 {
                    Belief::cold()
                } else {
                    flat((r.cpu_ms * 1_000 / r.wall_ms).max(1), r.peak_mem_mib)
                }
            })
            .collect();
        let beliefs = TableBeliefs(beliefs);
        let mut cfg = crate::engine::replay::Config::new(crate::engine::model::Pool::ci());
        cfg.warm_up_min = 5;
        let run = replay(
            &cfg,
            &rows,
            &EvenTruth::new(&rows),
            &beliefs,
            &mut LeastLoaded,
        );
        (rows, beliefs, run)
    })
}

/// Catches: an engine that only works on tidy hand traces. A slice of the committed
/// trace goes through on the CI pool: every row is accounted for, the books and the
/// bounds hold, cold and warm leases are both placed, and a second ranker consumes the
/// same arrivals.
#[test]
fn a_slice_of_the_committed_trace_replays_and_balances() {
    let (rows, beliefs, r) = smoke();
    let m = &r.metrics;
    let c = &m.counters;
    assert_eq!(c.submitted, 3_000);
    assert_eq!(
        c.completed + c.killed + c.queued_at_end + c.running_at_end,
        3_000
    );
    assert_eq!(m.check_books(), Ok(()));
    assert_eq!(c.check_a9(6, true), Ok(()));
    let cold: u64 = m.by_hour.values().map(|h| h.cold).sum();
    assert!(cold > 0 && cold < c.placed, "{cold} of {}", c.placed);
    assert!(m.metric_minutes > 0 && !m.window_ppm.is_empty());
    let cfg = {
        let mut cfg = crate::engine::replay::Config::new(crate::engine::model::Pool::ci());
        cfg.warm_up_min = 5;
        cfg
    };
    let other = replay(
        &cfg,
        rows,
        &EvenTruth::new(rows),
        beliefs,
        &mut Random::new(3),
    );
    assert_eq!(other.arrival_hash, r.arrival_hash);
    assert_eq!(other.metrics.check_books(), Ok(()));
}
