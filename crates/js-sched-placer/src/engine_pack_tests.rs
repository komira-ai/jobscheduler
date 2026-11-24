//! The pack limit in a walk over the queued jobs: a host is packed only to the limit (0.8 of
//! its cores at the default), and past it the least full host takes the job. Every host is of
//! 8 cores and 16 GiB and has no load admission, so it is booked to its 8000 m and no
//! further; 6400 m is 80% of its cores. Queued jobs book one core and 1 MiB, so CPU is the
//! binding share, and a host holds 6 of them within the limit (6000 m; a 7th would make
//! 7000 m, 87.5%) and 8 at most.

use std::collections::{BTreeMap, BTreeSet};

use js_jobs::Needs;
use js_types::{HostId, Resources};

use super::Engine;
use super::jobs::OpenHost;
use super::rig::{Tables, at, engine, host};

/// What a queued job books: one core.
const JOB_CPU: u64 = 1_000;

/// What a host's cores are: 8000 m.
const CORES: u64 = 8_000;

/// 80% of a host's cores: the default pack limit.
const LIMIT: u64 = CORES * 8 / 10;

/// Walks `count` new jobs of [`JOB_CPU`] over the hosts `names` in ONE walk: the host each
/// was placed on, in queue order.
fn burst(e: &mut Engine, names: &[&str], count: usize) -> Vec<HostId> {
    let tables = Tables::new();
    let needs = Needs::Shape(Resources::new(JOB_CPU, 1 << 20, 0, None).unwrap());
    let queued: Vec<_> = (0..count)
        .map(|n| tables.submit_with(&format!("new-{n:03}"), needs.clone(), 600))
        .collect();
    let open: BTreeMap<HostId, OpenHost> = names
        .iter()
        .map(|name| (host(name), OpenHost::default()))
        .collect();
    let (decided, waiting) = e.walk_jobs(&queued, &open, at(0));
    assert_eq!(waiting, Vec::new());
    let placed: BTreeMap<_, _> = decided
        .into_iter()
        .map(|(job, host, _, _)| (job, host))
        .collect();
    queued.iter().map(|job| placed[&job.id].clone()).collect()
}

/// Behaviour: a burst of 48 one-core jobs over six empty hosts of 8 cores, walked in one
/// call. Each host is packed to 80% of its cores before the next is started: the first 6
/// jobs (6000 m, the most that stays within 6400 m) go to one host, the next 6 to another,
/// and so on, so no host is booked past 6400 m while another is below it. Once all six are
/// at 6000 m the remaining 12 jobs go to the least full host each time, so the hosts end
/// level at 8 jobs (8000 m, their cores) and none is booked above its cores. Catches a pack
/// that fills one host to its cores while others idle, and a choice past the limit that is
/// not the least full host.
#[test]
fn a_burst_packs_a_host_to_eighty_percent_then_places_on_the_least_full() {
    let names = ["h1", "h2", "h3", "h4", "h5", "h6"];
    let none: Vec<(&str, &[u64])> = names.iter().map(|name| (*name, &[][..])).collect();
    let mut e = engine(&none);
    let chosen = burst(&mut e, &names, 48);

    let mut booked: BTreeMap<HostId, u64> = names.iter().map(|n| (host(n), 0)).collect();
    for (n, on) in chosen.iter().enumerate() {
        let before = booked[on];
        if n < 36 {
            assert!(
                before + JOB_CPU <= LIMIT,
                "job {n} on {on} after {before} m"
            );
            assert_eq!(on, &chosen[n / 6 * 6], "job {n} left the host it packs");
        } else {
            let least = booked.values().min().copied().unwrap();
            assert_eq!(before, least, "job {n} went to {on} at {before} m");
        }
        booked.insert(on.clone(), before + JOB_CPU);
    }
    let started: BTreeSet<_> = chosen.iter().step_by(6).take(6).collect();
    assert_eq!(started.len(), 6, "six hosts each start a run of 6");
    assert!(booked.values().all(|cpu| *cpu == CORES), "{booked:?}");
}

/// Behaviour: a host booked 7000 m of 8000 (87.5%, seven held jobs of one core) and one
/// booked 2000 m (25%) both fit a job of one core, but taking it puts the first at 100%,
/// past the limit, and the second at 37.5%, within it: the job goes to the second host,
/// whichever of the two names it has, so the choice is the pack limit's and not the names'
/// or the tie-break's. Catches a best fit that keeps packing the fullest host past the
/// limit.
#[test]
fn a_host_past_the_limit_is_passed_over_for_one_within_it() {
    let seven: Vec<u64> = (1..=7).collect();
    let two: Vec<u64> = (101..=102).collect();
    for (full, light) in [("a", "b"), ("b", "a")] {
        let mut e = engine(&[(full, &seven), (light, &two)]);
        let chosen = burst(&mut e, &["a", "b"], 1);
        assert_eq!(chosen, vec![host(light)], "full {full}");
    }
}
