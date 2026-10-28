//! Sim y: complementary profiles, scored minute by minute. Two 88-thread hosts, cap 96.8
//! cores. A runs 60 cores in minutes 0 to 4 (a busy phase of 0 to 2 dilated by 2) and 10
//! after; B runs a flat 30. The new lease has `p_bar` 32 cores: quiet charge 24 cores, 60
//! cores in the slots 5 to 15 (raw minutes 7 to 13, dilated). Exercises: the
//! minute-by-minute residual; key 1.

#[path = "support_score_y_ag/mod.rs"]
mod support;

use js_sched::Scorer;

use support::{CORES_88, Fake, Host, key_1_constant, pick, pick_with};

const MEMORY: u64 = 1 << 40;
/// Both hosts carry the same memory (87.5% of capacity) and the lease asks for none, so
/// the memory leftover is the same everywhere and the CPU shape alone decides.
const MEMORY_LOAD: u64 = MEMORY / 8 * 7;

fn a_load(slot: usize) -> u64 {
    if slot <= 4 { 60_000 } else { 10_000 }
}

fn b_load(_slot: usize) -> u64 {
    30_000
}

fn hosts(new: impl Fn(usize) -> u64 + Copy, life: u8) -> Fake {
    let a = Host::empty(CORES_88, MEMORY)
        .mem_load(|_| MEMORY_LOAD)
        .cpu_load(a_load)
        .cpu_new(new)
        .life(life);
    let b = Host::empty(CORES_88, MEMORY)
        .mem_load(|_| MEMORY_LOAD)
        .cpu_load(b_load)
        .cpu_new(new)
        .life(life);
    Fake::new(false, vec![("a", a), ("b", b)])
}

/// The lease with its learned curve: quiet 24 cores, 60 cores in the slots 5 to 15.
fn curve(slot: usize) -> u64 {
    if (5..=15).contains(&slot) {
        60_000
    } else {
        24_000
    }
}

/// The same busy minutes moved to the slots 0 to 6.
fn shuffled_curve(slot: usize) -> u64 {
    if slot <= 6 { 60_000 } else { 24_000 }
}

/// The same area charged flat: `p_bar` at every slot.
fn flat(_slot: usize) -> u64 {
    32_000
}

#[test]
fn the_lease_lands_on_the_host_with_the_idle_gap_not_the_one_idle_now() {
    let fake = hosts(curve, 16);
    let order = ["a", "b"];
    // Both pass the predicate: A peaks at 84 cores, B at 90, the cap is 96.8.
    assert!(fake.admits("a") && fake.admits("b"));
    let candidates = fake.admitted(&order);
    assert_eq!(candidates.len(), 2);
    // The previous rule looks at the load now: A 60, B 30. It picks B.
    assert_eq!(fake.least_loaded_now(&candidates), "b");
    // The score looks at every slot: B is 2 cores over 88 at the slots 5 to 15.
    assert_eq!(pick(&fake, &candidates), "a");
    // Candidate order does not matter.
    let reversed = fake.admitted(&["b", "a"]);
    assert_eq!(pick(&fake, &reversed), "a");
}

#[test]
fn busy_minutes_moved_to_the_start_fail_the_predicate_on_a_and_land_on_b() {
    let fake = hosts(shuffled_curve, 7);
    // A is 60 + 60 = 120 cores at minute 0, above 96.8. B is 30 + 60 = 90.
    assert!(!fake.admits("a"));
    assert!(fake.admits("b"));
    let candidates = fake.admitted(&["a", "b"]);
    assert_eq!(candidates.len(), 1);
    assert_eq!(pick(&fake, &candidates), "b");
}

#[test]
fn the_same_area_charged_flat_lands_on_b_because_a_would_run_over_88_cores() {
    let fake = hosts(flat, 1);
    // A is 60 + 32 = 92 cores (4.5% above 88, above the 1% tolerance), B is 62. Both pass.
    assert!(fake.admits("a") && fake.admits("b"));
    let candidates = fake.admitted(&["a", "b"]);
    assert_eq!(candidates.len(), 2);
    let mut scorer = Scorer::default();
    let choice = scorer
        .choose_among(&fake, &[&candidates[0], &candidates[1]])
        .expect("a choice");
    assert_eq!(choice.worker.as_str(), "b");
    assert!(!choice.slack_used, "B fits inside 100%");
    assert!(scorer.key(&fake, &candidates[0]) > scorer.key(&fake, &candidates[1]));
}

#[test]
fn with_key_1_constant_the_flat_lease_goes_to_the_fuller_host_that_does_not_fit() {
    // Control. Key 1 is the only thing that sees A's 92 cores: key 5 clamps the leftover
    // of an overfull slot to 0, so A looks like the perfect fit.
    let flat_case = hosts(flat, 1);
    let candidates = flat_case.admitted(&["a", "b"]);
    assert_eq!(pick(&flat_case, &candidates), "b");
    assert_eq!(
        pick_with(&mut key_1_constant(), &flat_case, &candidates),
        "a"
    );

    // Where the lease fits inside 100% on A at every slot, key 5 alone also picks A: key 1
    // is not what chose A there, it is what excludes B's slack in the flat case above.
    let curve_case = hosts(curve, 16);
    let candidates = curve_case.admitted(&["a", "b"]);
    assert_eq!(
        pick_with(&mut key_1_constant(), &curve_case, &candidates),
        "a"
    );
}

// ---- the shape alone: both hosts inside 100% at every slot, the same memory ----

/// Two 100-core hosts with memory used to the last byte, so the memory leftover is 0 at
/// every slot and the fit is the CPU leftover squared. The lease asks 10 cores at every
/// slot and lives `life` minutes. `a` and `b` give the CPU left on each host after the
/// lease at a slot, in cores.
fn shapes(life: u8, a: impl Fn(usize) -> u64, b: impl Fn(usize) -> u64) -> Fake {
    let host = |leftover: &dyn Fn(usize) -> u64| {
        Host::empty(100_000, MEMORY)
            .mem_load(|_| MEMORY)
            .cpu_load(|slot| 90_000 - leftover(slot) * 1_000)
            .cpu_new(|_| 10_000)
            .life(life)
    };
    let fake = Fake::new(false, vec![("a", host(&a)), ("b", host(&b))]);
    // Both pass the predicate and neither is over 100% anywhere: key 1 is 0 for both.
    assert!(fake.admits("a") && fake.admits("b"));
    let candidates = fake.admitted(&["a", "b"]);
    let choice = Scorer::default()
        .choose_among(&fake, &[&candidates[0], &candidates[1]])
        .expect("a choice");
    assert!(!choice.slack_used);
    fake
}

fn both_orders(fake: &Fake) -> String {
    let forward = pick(fake, &fake.admitted(&["a", "b"]));
    assert_eq!(forward, pick(fake, &fake.admitted(&["b", "a"])));
    forward
}

#[test]
fn the_leftover_at_every_minute_of_the_life_counts_not_only_minute_0() {
    // Leftovers over the three minutes of the life: A 10, 10, 40; B 2, 35, 35. Minute 0
    // alone prefers B (2 against 10); over the life A is the tighter fit.
    let fake = shapes(
        3,
        |s| [10, 10, 40].get(s).copied().unwrap_or(40),
        |s| [2, 35, 35].get(s).copied().unwrap_or(40),
    );
    assert_eq!(both_orders(&fake), "a");
}

#[test]
fn the_leftover_is_squared_so_one_large_gap_costs_more_than_two_small_ones() {
    // Over two minutes A leaves 20 and 20, B leaves 0 and 40. Added up as they are, B has
    // the smaller sum (30 * 40 = 1,200 against 61 * 20 = 1,220, weighted); squared, A's
    // 24,400 is half of B's 48,000.
    let fake = shapes(
        2,
        |s| if s < 2 { 20 } else { 40 },
        |s| match s {
            0 => 0,
            _ => 40,
        },
    );
    assert_eq!(both_orders(&fake), "a");
}

#[test]
fn a_gap_in_the_near_minutes_outweighs_a_slightly_larger_gap_in_the_far_ones() {
    // Thirty minutes of life. A leaves 30 in minutes 0 to 4 and nothing after; B leaves 31
    // in minutes 25 to 29 and nothing before. Unweighted B's squares are the larger sum
    // (961 against 900 each); weighted by the minutes' distance A's near gap is 145
    // against B's 20.
    let fake = shapes(
        30,
        |s| if s < 5 { 30 } else { 0 },
        |s| if (25..30).contains(&s) { 31 } else { 0 },
    );
    assert_eq!(both_orders(&fake), "b");
}
