//! The keyed draws: `splitmix64`, `mix` and the domain constants.

use std::collections::BTreeSet;

use crate::engine::tables::{LN15, LN32, LN2245, draw};
use crate::engine::truth::{
    DOMAIN_BURST, DOMAIN_CLASS, DOMAIN_MEM_SPIKE, DOMAIN_NOISE, DOMAIN_PERM, DOMAIN_RUN_MEAN,
    DOMAIN_SHIFT, DOMAINS, mix, splitmix64,
};

const GAMMA: u64 = 0x9E37_79B9_7F4A_7C15;

#[test]
fn splitmix64_matches_the_reference_generator() {
    // The first outputs of the published SplitMix64 generator seeded with 0: its state
    // steps by GAMMA and each output is the finaliser of the new state. `splitmix64(x)`
    // is the output for the state before the step. A wrong constant or shift changes them.
    assert_eq!(splitmix64(0), 0xE220_A839_7B1D_CDAF);
    assert_eq!(splitmix64(GAMMA), 0x6E78_9E6A_A1B9_65F4);
    assert_eq!(splitmix64(GAMMA.wrapping_mul(2)), 0x06C4_5D18_8009_454F);
    assert_eq!(splitmix64(u64::MAX), 0xE4D9_7177_1B65_2C20);
}

#[test]
fn mix_vectors_are_pinned() {
    // The keyed draws are the simulation's identity: a changed mix changes every number
    // in every report. These values come from the definition, chained splitmix64 over
    // seed, domain, subject and index.
    let cases = [
        ((0, 0, 0, 0), 0x2130_748A_AAC8_0268),
        ((1, 1, 0, 0), 0xC965_D634_8C18_8782),
        ((1, 4, 7, 3), 0x092D_2423_28F7_7BE9),
        ((0xDEAD_BEEF, 8, 1 << 63, 5), 0x5BE4_0DC9_B03F_9770),
    ];
    for ((seed, domain, subject, index), want) in cases {
        assert_eq!(mix(seed, domain, subject, index), want, "{seed} {domain}");
    }
}

#[test]
fn mix_depends_on_every_argument_and_their_order() {
    // An argument ignored, or two folded together by a plain xor, makes two quantities
    // that should be independent equal.
    let base = mix(1, 1, 1, 0);
    assert_eq!(base, 0x3EA0_A763_95D9_1588);
    assert_ne!(base, mix(2, 1, 1, 0));
    assert_ne!(base, mix(1, 2, 1, 0));
    assert_ne!(base, mix(1, 1, 2, 0));
    assert_ne!(base, mix(1, 1, 1, 1));
    // (subject, index) = (1, 0) and (0, 1) must differ, as must any swap.
    assert_eq!(mix(1, 1, 0, 1), 0x2DE5_A4E3_2AE6_D264);
    assert_ne!(mix(1, 1, 1, 0), mix(1, 1, 0, 1));
    assert_ne!(mix(1, 0, 1, 1), mix(1, 1, 0, 1));
    // The values a xor would confuse: (a, b) and (b, a) with the same xor.
    assert_ne!(mix(1, 2, 3, 0), mix(1, 3, 2, 0));
}

#[test]
fn domains_are_distinct_and_named() {
    // Two quantities sharing a domain would be perfectly correlated for one lease.
    let set: BTreeSet<u64> = DOMAINS.into_iter().collect();
    assert_eq!(set.len(), DOMAINS.len());
    assert_eq!(
        DOMAINS,
        [
            DOMAIN_CLASS,
            DOMAIN_SHIFT,
            DOMAIN_NOISE,
            DOMAIN_BURST,
            DOMAIN_MEM_SPIKE,
            DOMAIN_RUN_MEAN,
            DOMAIN_PERM
        ]
    );
    // And their values are part of the pinned numbers.
    assert_eq!(DOMAINS, [1, 2, 3, 4, 5, 6, 8]);
}

#[test]
fn table_draws_on_keyed_values_are_pinned() {
    // The path from a lease to a number: mix, then the table. Both ends pinned.
    assert_eq!(draw(&LN32, mix(1, DOMAIN_BURST, 7, 3)), 534_366);
    assert_eq!(draw(&LN32, mix(2, DOMAIN_BURST, 7, 3)), 936_664);
    assert_eq!(draw(&LN15, mix(1, DOMAIN_NOISE, 7, 3)), 1_111_350);
    assert_eq!(draw(&LN15, mix(2, DOMAIN_NOISE, 7, 3)), 1_055_294);
    assert_eq!(draw(&LN2245, mix(1, DOMAIN_RUN_MEAN, 7, 3)), 1_093_011);
    assert_eq!(draw(&LN2245, mix(2, DOMAIN_RUN_MEAN, 7, 3)), 984_142);
}

#[test]
fn mix_outputs_look_uniform_in_the_bits_the_tables_read() {
    // A mix whose high bits are biased skews every table draw: the top 20 bits over
    // 100,000 consecutive subjects fill each of 16 buckets within 10% of even.
    let mut buckets = [0u32; 16];
    for subject in 0..100_000 {
        buckets[(mix(1, DOMAIN_BURST, subject, 0) >> 60) as usize] += 1;
    }
    assert!(
        buckets.iter().all(|&n| (5_600..=6_900).contains(&n)),
        "{buckets:?}"
    );
}
