//! Sim ah: the utilization lever. The pool's ground-truth per-run means are drawn from the
//! lognormal of the design's Utilization section (median / P90 = 0.75, `sigma = 0.2245`)
//! and every run is predicted at the distribution's P90. Admitted at that prediction, a
//! host whose predicted load is held at 100% of `C` runs at about `E[k] = 0.77 C`, the
//! model's p99 of `k` (actual over predicted) is about 1.26, and no placement is planned
//! above `(1 + s_cpu) C`. This checks the arithmetic of the model, not the farm.
//! Exercises: the percentile setting (predicted at the median instead, the same pool runs
//! at about `C`).
//!
//! The draws are a fixed sequence (SplitMix64 and Box-Muller from a constant seed), so the
//! sim replays exactly. Time is the model's logical ticks; no assertion reads a clock.

#[path = "load_world/mod.rs"]
mod load_world;

use js_sched::{LoadConfig, Prediction};
use js_types::sched::WorkerId;

use load_world::{GIB, Spec, World};

/// The pool: four 88-thread hosts.
const HOSTS: [&str; 4] = ["a", "b", "c", "d"];
const CORES: u64 = 88;
/// `C`, in millicores.
const C: u64 = CORES * 1_000;
/// Runs drawn; 50 rounds of the pool's 88 concurrent runs at the P90.
const RUNS: usize = 4_400;
/// Every run lasts four windows at its own mean (it is never slowed: see the tests).
const RUN_TICKS: u64 = 20;
/// The spread of the log of a key's per-run mean: median / P90 = 0.75.
const SIGMA: f64 = 0.2245;
/// The 90th percentile of the standard normal.
const Z90: f64 = 1.281_551_6;
/// The median per-run mean, in millicores.
const MEDIAN: f64 = 3_000.0;
const SEED: u64 = 0x5eed_00a4;
/// Queued runs kept ahead of the pool: more than it starts in one round, so that every
/// host is refilled in the round after its runs end.
const QUEUED: usize = 96;

/// SplitMix64: a fixed sequence of 64-bit draws.
struct Draws(u64);

impl Draws {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// A draw in `(0, 1]`.
    fn unit(&mut self) -> f64 {
        ((self.next_u64() >> 11) + 1) as f64 / (1u64 << 53) as f64
    }

    /// A standard normal draw (Box-Muller).
    fn normal(&mut self) -> f64 {
        let (u1, u2) = (self.unit(), self.unit());
        (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
    }
}

/// The ground-truth per-run means of the pool, in millicores, in submission order.
fn per_run_means() -> Vec<u64> {
    let mut draws = Draws(SEED);
    (0..RUNS)
        .map(|_| (MEDIAN * (SIGMA * draws.normal()).exp()).round() as u64)
        .collect()
}

/// A percentile of the distribution, in millicores: `z` standard deviations of the log
/// from the median.
fn percentile(z: f64) -> u64 {
    (MEDIAN * (SIGMA * z).exp()).round() as u64
}

fn warm(cpu_millis: u64) -> Prediction {
    Prediction {
        cpu_millis,
        memory_bytes: GIB,
        cold: false,
        life_minutes: 1,
    }
}

/// What a run of the pool measured.
struct Outcome {
    /// The mean of the ground-truth CPU / `C` over the binding host-ticks: those at which
    /// the host's predicted load leaves no room for one more run.
    mean_binding: f64,
    /// How many host-ticks were binding.
    binding: usize,
    /// The 99th percentile of `k`, a run's ground-truth mean over its prediction, over
    /// the runs that ran.
    p99_k: f64,
    /// The most predicted CPU any host was planned at, in millicores.
    planned_max: u64,
    /// The most ground-truth demand any host had at the end of a tick, in millicores.
    demand_max: u64,
    started: u64,
}

/// The pool with every run predicted at `predict` millicores, under the CPU slack
/// `s_cpu_permille`.
fn run(predict: u64, s_cpu_permille: u32) -> Outcome {
    let config = LoadConfig {
        s_cpu_permille,
        ..LoadConfig::default()
    };
    let cap = C * u64::from(1_000 + s_cpu_permille) / 1_000;
    let mut w = World::new(config);
    for host in HOSTS {
        w.linux(host, CORES, 256);
    }
    // Two readings in: every host has measured.
    w.run(10);
    let means = per_run_means();
    let mut next = 0;
    let mut binding = Vec::new();
    let mut planned_max = 0;
    let mut demand_max = 0;
    for _ in 0..20_000 {
        while next < RUNS && w.s.queued_len() < QUEUED {
            w.submit(&Spec::flat("run", means[next], RUN_TICKS, warm(predict)));
            next += 1;
        }
        w.step();
        for host in HOSTS {
            let view =
                w.s.load_view(&WorkerId::new(host))
                    .expect("a host of the pool");
            let demand = w.demand_on(host);
            planned_max = planned_max.max(view.predicted_cpu_millis);
            demand_max = demand_max.max(demand);
            if view.predicted_cpu_millis + predict > cap {
                binding.push(demand as f64 / C as f64);
            }
        }
        if next == RUNS && w.stats["run"].finished == RUNS as u64 {
            break;
        }
    }
    let stats = &w.stats["run"];
    assert_eq!(stats.finished, RUNS as u64, "drained: {stats:?}");
    assert_eq!(stats.killed, 0);
    let mut k: Vec<f64> = means.iter().map(|&m| m as f64 / predict as f64).collect();
    k.sort_by(f64::total_cmp);
    let p99 = (k.len() * 99).div_ceil(100) - 1;
    Outcome {
        mean_binding: binding.iter().sum::<f64>() / binding.len() as f64,
        binding: binding.len(),
        p99_k: k[p99],
        planned_max,
        demand_max,
        started: stats.started,
    }
}

fn report(name: &str, o: &Outcome) {
    println!(
        "sim ah {name}: mean CPU/C {:.4} over {} binding host-ticks, p99 k {:.4}, \
         planned at most {} m, demand at most {} m, {} runs",
        o.mean_binding, o.binding, o.p99_k, o.planned_max, o.demand_max, o.started
    );
}

/// The premise: the distribution's P90 is 4 cores (its median is 3), and the draws have
/// the distribution's median and spread.
#[test]
fn the_draws_are_the_lognormal_of_the_utilization_section() {
    assert_eq!(percentile(Z90), 4_000);
    assert_eq!(percentile(0.0), 3_000);
    let mut means = per_run_means();
    means.sort_unstable();
    let median = means[RUNS / 2];
    let p90 = means[RUNS * 9 / 10];
    assert!((2_940..=3_060).contains(&median), "median {median}");
    assert!((3_920..=4_080).contains(&p90), "P90 {p90}");
}

/// Sim ah. Catches: a predicate that plans a host above `(1 + s_cpu) C`, and a model whose
/// figures are not the design's. With `s_cpu = 0` the predicted load is held at exactly
/// `C` (22 runs of 4 cores on 88): the ground-truth CPU over the binding host-ticks is
/// within 0.03 of 0.77 C, the p99 of `k` within 0.05 of 1.26, and no host is planned
/// above `C`. The pool's demand never passes `C` there, so no run is slowed and the
/// ground truth is each run's own mean.
#[test]
fn ah_admitted_at_the_p90_a_host_held_at_c_runs_at_about_0_77_c() {
    let o = run(percentile(Z90), 0);
    report("P90, s_cpu 0", &o);
    assert!(o.planned_max <= C, "planned at {} m", o.planned_max);
    assert_eq!(o.planned_max, C, "held at 100% of C");
    assert!(o.demand_max <= C, "a run was slowed: {} m", o.demand_max);
    assert!(o.binding > 0);
    assert!(
        (o.mean_binding - 0.77).abs() <= 0.03,
        "mean CPU/C {}",
        o.mean_binding
    );
    assert!((o.p99_k - 1.26).abs() <= 0.05, "p99 k {}", o.p99_k);
}

/// Sim ah at the default slack. Catches: a placement planned above `(1 + s_cpu) C =
/// 96.8` cores. At the cap (24 runs of 4 cores, 96 cores) the ground-truth CPU is the
/// design's 0.85 C (0.77 times 1.1), within 0.03.
#[test]
fn ah_at_the_default_slack_nothing_is_planned_above_1_1_c() {
    let o = run(percentile(Z90), 100);
    report("P90, s_cpu 0.10", &o);
    assert!(
        o.planned_max <= C * 11 / 10,
        "planned at {} m",
        o.planned_max
    );
    assert_eq!(o.planned_max, 96_000, "24 runs of 4 cores");
    assert!(
        (o.mean_binding - 0.85).abs() <= 0.03,
        "mean CPU/C {}",
        o.mean_binding
    );
    assert!((o.p99_k - 1.26).abs() <= 0.05, "p99 k {}", o.p99_k);
}

/// Control: the same pool predicted at the median (3 cores) instead of the P90. The
/// predicate still plans nothing above `C`, but the ground truth over the binding
/// host-ticks is near `C` (29 runs of 3 cores at `E[k] = 1.03`, a little less where the
/// measurement holds the 29th back), far outside 0.77 plus 0.03: the 0.77 is the
/// percentile's doing.
#[test]
fn ah_control_predicted_at_the_median_the_same_pool_runs_at_about_c() {
    let o = run(percentile(0.0), 0);
    report("median, s_cpu 0", &o);
    assert!(o.planned_max <= C, "planned at {} m", o.planned_max);
    assert!(
        o.mean_binding > 0.77 + 0.03,
        "mean CPU/C {}",
        o.mean_binding
    );
    assert!(o.mean_binding > 0.9, "mean CPU/C {}", o.mean_binding);
    assert!(o.p99_k > 1.26 + 0.05, "p99 k {}", o.p99_k);
}
