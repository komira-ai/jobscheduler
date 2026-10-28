//! The two rankers of this series: the previous rule and the random one.

use super::flat;
use crate::engine::model::{HostState, Pool};
use crate::engine::predicate::Params;
use crate::engine::rankers::{
    Choose, Load, Pick, Random, Ranker, least_loaded_standin, splitmix64, stream_seed,
};

/// The flat rule's `Load::new` with no lease cap: `(booked, capacity)` of each resource.
fn load(cpu: (u64, u64), mem: (u64, u64), leases: usize) -> Load {
    Load::new(cpu.0, mem.0, (cpu.1, mem.1, 0), leases)
}

const GIB: u64 = 1 << 30;

/// Catches: a share compared by division in floats, or by a product that overflows: the
/// shares one part in `u64::MAX` apart differ and equal fractions of different capacities
/// are equal (a tie falls to the lease count, not to rounding). Restated from the flat
/// rule's load cases.
#[test]
fn shares_compare_exactly() {
    let just_under = load((u64::MAX - 1, u64::MAX), (0, GIB), 0);
    let full = load((u64::MAX, u64::MAX), (0, GIB), 0);
    assert!(just_under < full);
    assert_eq!(load((1, 3), (0, GIB), 0), load((2_000, 6_000), (0, GIB), 0));
    assert!(load((1, 3), (0, GIB), 0) < load((34, 100), (0, GIB), 0));
}

/// Catches: only one resource ranking a host: the load is the larger share, so a host at
/// half its cores and three quarters of its memory is as loaded as one at three quarters
/// of its cores and a sixteenth of its memory, and more than one at half of each. (Restated
/// from the flat rule.)
#[test]
fn the_larger_of_the_cpu_and_memory_shares_is_the_load() {
    let cpu_bound = load((750, 1_000), (GIB, 16 * GIB), 0);
    let memory_bound = load((500, 1_000), (12 * GIB, 16 * GIB), 0);
    let both_half = load((500, 1_000), (8 * GIB, 16 * GIB), 0);
    assert_eq!(cpu_bound, memory_bound);
    assert!(both_half < memory_bound);
}

/// Catches: a share capped at 1, which ties every host booked past its memory.
#[test]
fn a_share_past_one_still_ranks() {
    let a = load((1_000, 8_000), (24 * GIB, 16 * GIB), 4);
    let b = load((1_000, 8_000), (16 * GIB, 16 * GIB), 4);
    assert!(b < a);
}

/// Catches: a resource with no capacity dividing by zero, or counting.
#[test]
fn a_resource_without_capacity_counts_for_nothing() {
    let no_memory = load((500, 1_000), (7 * GIB, 0), 1);
    assert_eq!(no_memory, load((500, 1_000), (0, 0), 1));
    assert!(no_memory < load((501, 1_000), (0, 0), 1));
    assert_eq!(load((9, 0), (9, 0), 0), load((0, 5), (0, 5), 0));
}

/// Catches: a capped host ranked by its CPUs alone: a 32-core host capped at 2 leases that
/// holds 1 is half used, more than a peer at 10 of 32 cores, and a host at its cap is more
/// loaded than any under it. A host with no cap, or a cap of 0, is ranked as before.
#[test]
fn a_capped_host_is_ranked_by_its_lease_share_too() {
    let capped = |cap: Option<u64>, leases: usize| {
        Load::new(
            leases as u64 * 1_000,
            0,
            (32_000, 0, cap.unwrap_or(0)),
            leases,
        )
    };
    let peer = load((10_000, 32_000), (0, 0), 10);
    assert!(peer < capped(Some(2), 1));
    assert!(capped(Some(2), 1) < capped(Some(2), 2));
    assert_eq!(capped(None, 1), load((1_000, 32_000), (0, 0), 1));
    assert_eq!(capped(Some(0), 1), capped(None, 1));
}

/// Catches: leases ignored when the shares tie, or counted before the shares.
#[test]
fn a_tie_in_shares_goes_to_fewer_leases() {
    let few = load((500, 1_000), (GIB, 16 * GIB), 1);
    let many = load((250, 500), (GIB, 16 * GIB), 2);
    assert!(few < many);
    assert!(load((1, 10), (0, GIB), 9) < load((2, 10), (0, GIB), 0));
}

/// Catches: a tie broken by anything but position: equal loads give the first listed (name
/// order, as shipped), the least loaded wins wherever it is, and no hosts give none.
#[test]
fn the_first_of_equals_wins() {
    let same = load((500, 1_000), (0, 0), 1);
    let lighter = load((400, 1_000), (0, 0), 1);
    assert_eq!(
        least_loaded_standin(&[(3, same), (1, same), (2, same)]),
        Some(3)
    );
    assert_eq!(least_loaded_standin(&[(1, same), (3, same)]), Some(1));
    assert_eq!(
        least_loaded_standin(&[(1, same), (9, lighter), (2, lighter)]),
        Some(9)
    );
    assert_eq!(least_loaded_standin(&[]), None);
}

/// Four empty hosts and the lease's belief, for choosing among `candidates`.
fn choose_among<R: Ranker>(r: &mut R, lease: usize, candidates: &[usize]) -> usize {
    let cap = Pool::ci().cap;
    let hosts: Vec<HostState<'_>> = (0..4).map(|_| HostState::new(cap)).collect();
    let belief = flat(1_000, 100);
    let params = Params::default();
    let pick: Pick = r.choose(&Choose {
        lease,
        tick: 0,
        belief: &belief,
        hosts: &hosts,
        candidates,
        params: &params,
    });
    assert!(candidates.contains(&pick.host));
    pick.host
}

/// Catches: the mixing function or the stream key changing under a seed that a recorded
/// result depends on.
#[test]
fn the_mixing_is_pinned() {
    assert_eq!(splitmix64(0), 0xe220_a839_7b1d_cdaf);
    assert_eq!(splitmix64(1), 0x910a_2dec_8902_5cc1);
    assert_eq!(stream_seed(1, 0), 0x0e11_5d98_7dae_f206);
    assert_eq!(stream_seed(1, 1), 0x66e8_a074_1cfe_d008);
    assert_eq!(stream_seed(2, 0), 0x53eb_b876_c62d_abe5);
}

/// Catches: a random ranker that depends on the order candidates are handed in (the
/// draws are per host of the pool, not per position), that never moves off one host, or
/// that shares one seed's draws across leases.
#[test]
fn the_random_ranker_depends_on_who_passed_and_not_on_their_order() {
    let mut forward = Random::new(5);
    let mut backward = Random::new(5);
    let mut picks = Vec::new();
    for lease in 0..60 {
        let a = choose_among(&mut forward, lease, &[0, 1, 2, 3]);
        let b = choose_among(&mut backward, lease, &[3, 2, 1, 0]);
        assert_eq!(a, b, "lease {lease}");
        picks.push(a);
    }
    for host in 0..4 {
        let n = picks.iter().filter(|&&p| p == host).count();
        assert!(n >= 5, "host {host} chosen {n} times of 60");
    }
    // A subset: the host chosen is the one of them with the smallest draw, so it is the
    // same as among all four whenever that one is in the subset.
    let mut all = Random::new(5);
    let mut some = Random::new(5);
    for lease in 0..60 {
        let a = choose_among(&mut all, lease, &[0, 1, 2, 3]);
        let b = choose_among(&mut some, lease, &[0, 1, 2]);
        if a != 3 {
            assert_eq!(a, b, "lease {lease}");
        }
    }
}

/// Catches: one stream for the whole ranker (a lease's draws shifting when another lease
/// is looked at in between) or a stream that is not restarted when the lease is placed.
#[test]
fn each_lease_has_a_stream_of_its_own() {
    let looks = |interleave: bool| {
        let mut r = Random::new(11);
        let mut seq = Vec::new();
        for i in 0..30 {
            if interleave {
                choose_among(&mut r, 99, &[0, 1, 2, 3]);
                if i % 3 == 0 {
                    r.placed(99);
                }
            }
            seq.push(choose_among(&mut r, 7, &[0, 1, 2, 3]));
        }
        seq
    };
    let alone = looks(false);
    assert_eq!(
        alone,
        looks(true),
        "another lease shifted this lease's draws"
    );
    assert!(
        alone.iter().any(|&h| h != alone[0]),
        "a fresh draw each time it is looked at"
    );

    let mut r = Random::new(11);
    let first = choose_among(&mut r, 7, &[0, 1, 2, 3]);
    choose_among(&mut r, 7, &[0, 1, 2, 3]);
    r.placed(7);
    assert_eq!(
        choose_among(&mut r, 7, &[0, 1, 2, 3]),
        first,
        "restarted after placement"
    );

    // Another seed gives another sequence.
    let mut other = Random::new(12);
    let seq: Vec<usize> = (0..30)
        .map(|_| choose_among(&mut other, 7, &[0, 1, 2, 3]))
        .collect();
    assert_ne!(seq, alone);
}
