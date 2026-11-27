//! Sim ac: same-key concentration. One fan-out submits 30 leases of one action key on a pool
//! of six 88-thread hosts. The key has a learned curve (every lease the same: 4 cores and
//! 1 GiB a minute for 5 minutes), and its real demand is twice the prediction (`k = 2`, 8
//! cores a lease), with every lease starting in the same round. Best fit alone (key 5)
//! packs a key's leases onto the fullest host, so a misprediction of the key lands on one
//! host at once; key 4 moves a lease off a host once the key would pass its share of the
//! host's CPU (`--placement-key-share`, 25% by default: 22 cores, five leases of 4).
//! Exercises: key 4.
//!
//! Logical ticks only (a tick is a second); no assertion reads a clock.

#[path = "load_curves/mod.rs"]
mod load_curves;
#[path = "load_world/mod.rs"]
mod load_world;

use std::collections::BTreeMap;

use js_sched::{LoadConfig, Prediction};

use load_curves::Curves;
use load_world::{GIB, Spec, World};

const HOSTS: [&str; 6] = ["a", "b", "c", "d", "e", "f"];
const CORES: u64 = 88;
/// `C_cpu`, in millicores.
const C: u64 = CORES * 1_000;
const FAN_OUT: u64 = 30;
/// The key's prediction and its curve, in millicores a lease.
const PREDICTED: u64 = 4_000;
/// The real demand: `k = 2`.
const REAL: u64 = 2 * PREDICTED;
/// Five minutes.
const TICKS: u64 = 300;

fn warm(cpu_millis: u64) -> Prediction {
    Prediction {
        cpu_millis,
        memory_bytes: GIB,
        cold: false,
        life_minutes: 5,
    }
}

/// A lease of the fan-out: it books and is predicted at 4 cores and really asks 8.
fn leaf() -> Spec {
    Spec {
        key: Some("fan"),
        request_cpu: PREDICTED,
        mem: vec![(TICKS, GIB)],
        ..Spec::flat("fan", REAL, TICKS, warm(PREDICTED))
    }
}

/// What the fan-out did on each host.
struct Spread {
    /// The leases of the key each host holds, by host.
    leases: BTreeMap<&'static str, u64>,
    /// The 99th percentile of the ground-truth CPU / `C` of each host over the fan-out's
    /// run.
    p99: BTreeMap<&'static str, f64>,
    killed: u64,
    concentrated_choices: u64,
    profile_cpu: u64,
}

impl Spread {
    /// The predicted cores of the key on `host`, in millicores.
    fn key_millis(&self, host: &str) -> u64 {
        self.leases[host] * PREDICTED
    }
}

/// The fan-out on the pool under `key_share_percent`.
fn fan_out(key_share_percent: u32) -> Spread {
    let config = LoadConfig {
        key_share_percent,
        ..LoadConfig::default()
    };
    let curves = Curves::new("sim");
    let mut w = World::with_curves(config, curves.clone());
    for host in HOSTS {
        w.linux(host, CORES, 512);
    }
    curves.set("fan", &[(4_000, GIB); 5], 5);
    curves.set_life("fan", 5);
    // Two readings in: every host has measured, and all are equal on keys 1 to 3.
    w.run(10);
    // One operation each: a fan-out's leaves are not joined into one run.
    let leaf = leaf();
    for _ in 0..FAN_OUT {
        w.submit_unjoined(&leaf);
    }
    let at = w.tick;
    w.step();
    let started: Vec<_> = w.starts.iter().filter(|(_, c, ..)| *c == "fan").collect();
    assert_eq!(started.len() as u64, FAN_OUT, "the fan-out in one round");
    assert!(started.iter().all(|(t, ..)| *t == at), "start-aligned");
    let mut leases: BTreeMap<&'static str, u64> = HOSTS.iter().map(|h| (*h, 0)).collect();
    for (_, _, node, ..) in &started {
        *leases.get_mut(node.as_str()).expect("a host of the pool") += 1;
    }
    let counters = w.s.load_counters();
    // The ground-truth demand of each host at every tick of the fan-out's run (the first
    // tick's sharing has happened in the step above).
    let mut demand: BTreeMap<&'static str, Vec<u64>> =
        HOSTS.iter().map(|h| (*h, vec![w.demand_on(h)])).collect();
    while w.stats["fan"].finished + w.stats["fan"].killed < FAN_OUT {
        assert!(w.tick < at + 2 * TICKS, "the fan-out ends: {:?}", w.stats);
        for host in HOSTS {
            demand
                .get_mut(host)
                .expect("a host")
                .push(w.demand_on(host));
        }
        w.step();
    }
    let p99 = demand
        .into_iter()
        .map(|(host, mut ticks)| {
            ticks.sort_unstable();
            let i = (ticks.len() * 99).div_ceil(100) - 1;
            (host, ticks[i] as f64 / C as f64)
        })
        .collect();
    Spread {
        leases,
        p99,
        killed: w.stats["fan"].killed,
        concentrated_choices: counters.chose_concentrated,
        profile_cpu: counters.profile_cpu,
    }
}

/// Whether no host holds more than `share_percent` of its CPU of the key while another host
/// (all are equal on keys 1 to 3 here) holds less.
fn within_share(spread: &Spread, share_percent: u64) -> bool {
    let share = C * share_percent / 100;
    let over = HOSTS.iter().any(|h| spread.key_millis(h) > share);
    let under = HOSTS.iter().any(|h| spread.key_millis(h) < share);
    !(over && under)
}

fn report(name: &str, spread: &Spread) {
    for host in HOSTS {
        println!(
            "sim ac {name}: host {host}: {} leases of the key, p99 CPU/C {:.3}",
            spread.leases[host], spread.p99[host]
        );
    }
    println!("sim ac {name}: {} killed", spread.killed);
}

/// Sim ac. Catches: key 4 missing or not updated within a round, so that the fit packs the
/// fan-out onto the fullest hosts. Under the default share of 25% every host takes five
/// leases (20 cores; a sixth would make 24, above 22) and none is above the share while
/// another is below; at `k = 2` each host runs at 40 cores, a p99 of 0.45 C, at most the
/// key share times `k` (0.5 C), and nothing is killed. No placement had to take a host
/// past the share, and every lease was charged by its curve.
#[test]
fn ac_a_fan_out_of_one_key_is_held_to_the_key_share_of_each_host() {
    let spread = fan_out(25);
    report("share 25%", &spread);
    assert!(within_share(&spread, 25), "{:?}", spread.leases);
    assert!(
        HOSTS.iter().all(|h| spread.leases[h] == 5),
        "{:?}",
        spread.leases
    );
    for host in HOSTS {
        assert!(
            spread.p99[host] <= 0.5,
            "host {host} p99 CPU/C {}",
            spread.p99[host]
        );
    }
    assert_eq!(spread.killed, 0);
    assert_eq!(spread.concentrated_choices, 0);
    assert_eq!(spread.profile_cpu, FAN_OUT);
}

/// Control: with `--placement-key-share=100` key 4 never prefers another host, the fit
/// packs the key onto the fullest host up to the pack limit (eleven leases, 44 cores of
/// 88), and the assertion above fails: a host holds more than 25% of its CPU of the key
/// while another holds less, and at `k = 2` it runs at its cores (p99 1.0 C, above 0.5).
#[test]
fn ac_control_with_a_key_share_of_100_percent_the_fan_out_packs_and_the_assertion_fails() {
    let spread = fan_out(100);
    report("share 100%", &spread);
    assert!(!within_share(&spread, 25), "{:?}", spread.leases);
    let most = HOSTS
        .iter()
        .max_by_key(|h| (spread.leases[**h], std::cmp::Reverse(**h)))
        .expect("a host");
    assert!(spread.leases[most] > 5, "{:?}", spread.leases);
    assert!(
        spread.p99[most] > 0.5,
        "host {most} p99 CPU/C {}",
        spread.p99[most]
    );
    assert_eq!(spread.killed, 0);
}
