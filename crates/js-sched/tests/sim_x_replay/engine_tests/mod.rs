//! Tests of the replay engine: the predicate, the metrics and the loop.
//!
//! Each test names the defect it would catch. None reads a clock or asserts a timing.
//! Worlds are hand-made (or a slice of the committed trace) and use stand-ins for the
//! two seams ([`EvenTruth`], [`TableBeliefs`]) so nothing here depends on the truth model
//! or the learning predictor.

use std::sync::OnceLock;

use crate::engine::fixture::{Fixture, KeyIx, Outcome, Row};
use crate::engine::model::{Belief, Beliefs, Charge, Pool, Truth};
use crate::engine::rankers::{LeastLoaded, Ranker};
use crate::engine::replay::{Config, Replay, replay};

mod hand;
mod metrics;
mod predicate;
mod rankers;

/// A row that ends ok.
pub fn row(start_ms: u64, wall_ms: u64, cpu_ms: u64, mem_mib: u64) -> Row {
    Row {
        start_ms,
        key: KeyIx(0),
        wall_ms,
        cpu_ms,
        peak_mem_mib: mem_mib,
        outcome: Outcome::Ok,
    }
}

/// Truth that spreads a row's `cpu_ms` evenly over its seconds (the remainder on the
/// first ones, so the seconds add up exactly) and holds a working set per lease.
pub struct EvenTruth {
    rows: Vec<Row>,
    /// The extra core-ms burnt in the first second of every lease (0 for an honest truth).
    pub extra_first_tick: u64,
}

impl EvenTruth {
    /// Truth for `rows`.
    pub fn new(rows: &[Row]) -> Self {
        Self {
            rows: rows.to_vec(),
            extra_first_tick: 0,
        }
    }
}

impl Truth for EvenTruth {
    fn core_ms(&self, lease: usize, age_tick: u64) -> u64 {
        let r = &self.rows[lease];
        let n = r.wall_ms.div_ceil(1_000);
        let extra = if age_tick == 0 {
            self.extra_first_tick
        } else {
            0
        };
        r.cpu_ms / n + u64::from(age_tick < r.cpu_ms % n) + extra
    }

    fn working_set_mib(&self, lease: usize, _age_tick: u64) -> u64 {
        self.rows[lease].peak_mem_mib
    }
}

/// One belief per lease.
pub struct TableBeliefs(pub Vec<Belief>);

impl TableBeliefs {
    /// The same belief for each of `n` leases.
    pub fn uniform(n: usize, belief: &Belief) -> Self {
        Self(vec![belief.clone(); n])
    }
}

impl Beliefs for TableBeliefs {
    fn belief(&self, lease: usize) -> &Belief {
        &self.0[lease]
    }
}

/// A flat warm belief of `cores` and `mem_mib`.
pub fn flat(cpu_millis: u64, mem_mib: u64) -> Belief {
    Belief::flat(Charge::new(cpu_millis, mem_mib))
}

/// A replay of `rows` with even truth and the given beliefs.
pub fn run(cfg: &Config, rows: &[Row], beliefs: &TableBeliefs, ranker: &mut dyn Ranker) -> Replay {
    replay(cfg, rows, &EvenTruth::new(rows), beliefs, ranker)
}

/// A pool of `hosts` hosts of `threads` threads and `mem_mib` MiB, no warm-up.
pub fn small_config(hosts: usize, threads: u64, mem_mib: u64) -> Config {
    let mut cfg = Config::new(Pool::new(hosts, threads, mem_mib));
    cfg.warm_up_min = 0;
    cfg
}

/// The busy world several tests share: a row every 3 s for 6 hours on two 4-thread hosts,
/// 4 to 8 s long at 1.5 cores, every tenth lease cold.
pub struct BusyWorld {
    pub cfg: Config,
    pub rows: Vec<Row>,
    pub beliefs: TableBeliefs,
    pub run: Replay,
}

/// The busy world, built once for every test that reads it (a pure function of nothing).
pub fn busy() -> &'static BusyWorld {
    static WORLD: OnceLock<BusyWorld> = OnceLock::new();
    WORLD.get_or_init(|| {
        let mut cfg = small_config(2, 4, 16_384);
        cfg.warm_up_min = 5;
        let rows: Vec<Row> = (0..7_200u64)
            .map(|i| {
                let wall = 4_000 + (i % 5) * 1_000;
                row(i * 3_000, wall, wall * 3 / 2, 512)
            })
            .collect();
        let beliefs = TableBeliefs(
            (0..rows.len())
                .map(|i| {
                    if i % 10 == 0 {
                        Belief::cold()
                    } else {
                        flat(2_000, 600)
                    }
                })
                .collect(),
        );
        let run = run(&cfg, &rows, &beliefs, &mut LeastLoaded);
        BusyWorld {
            cfg,
            rows,
            beliefs,
            run,
        }
    })
}

/// The committed fixture, parsed once.
pub fn embedded() -> &'static Fixture {
    static F: OnceLock<Fixture> = OnceLock::new();
    F.get_or_init(|| Fixture::embedded().expect("the committed fixture loads"))
}
