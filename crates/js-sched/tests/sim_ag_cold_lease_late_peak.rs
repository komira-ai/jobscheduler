//! Sim ag: a cold lease and a late peak. 88-thread hosts, 64 GiB each. A is at 70% of CPU
//! now and its own predicted load reaches 108% (with the lease) at minute 10; B is at 80%
//! flat. The cold lease is charged flat: 2 cores and 4 GiB at every slot, `L = 1`.
//! Exercises: key 1 over every predicate slot.
//!
//! The row gives CPU figures only. On CPU alone the fit at `tau = 0` prefers the fuller
//! host, B, so the control (scoring at `tau = 0` only picks A) needs A to be the better
//! fit now: A is 50 GiB full in memory and B 10 GiB, which leaves B's memory stranded.

#[path = "support_score_y_ag/mod.rs"]
mod support;

use support::{AtSlotZero, CORES_88, Fake, GIB, Host, key_1_constant, pick, pick_with};

const MEMORY: u64 = 64 * GIB;
const FLAT_CPU: u64 = 2_000;
const FLAT_MEMORY: u64 = 4 * GIB;
/// A's load without the lease at minute 10, so that with the lease it is 108% of 88 cores.
const A_PEAK: u64 = CORES_88 * 108 / 100 - FLAT_CPU;

fn a_cpu(slot: usize) -> u64 {
    if slot == 10 {
        A_PEAK
    } else {
        CORES_88 * 70 / 100
    }
}

fn fake() -> Fake {
    let a = Host::empty(CORES_88, MEMORY)
        .cpu_load(a_cpu)
        .mem_load(|_| 50 * GIB)
        .cpu_new(|_| FLAT_CPU)
        .mem_new(|_| FLAT_MEMORY);
    let b = Host::empty(CORES_88, MEMORY)
        .cpu_load(|_| CORES_88 * 80 / 100)
        .mem_load(|_| 10 * GIB)
        .cpu_new(|_| FLAT_CPU)
        .mem_new(|_| FLAT_MEMORY);
    Fake::new(true, vec![("a", a), ("b", b)])
}

#[test]
fn a_cold_lease_goes_to_b_because_a_is_108_percent_at_minute_ten() {
    let fake = fake();
    // Both pass the predicate (A peaks at 95.04 cores against the cap of 96.8).
    assert!(fake.admits("a") && fake.admits("b"));
    let candidates = fake.admitted(&["a", "b"]);
    assert_eq!(candidates.len(), 2);
    // The previous rule, the load now: A is at 70%, B at 80%.
    assert_eq!(fake.least_loaded_now(&candidates), "a");
    assert_eq!(pick(&fake, &candidates), "b");
    let reversed = fake.admitted(&["b", "a"]);
    assert_eq!(pick(&fake, &reversed), "b");
}

#[test]
fn scoring_at_minute_zero_only_would_pick_a() {
    // Control: every slot read as slot 0. A is the better fit now and the peak is not seen.
    let fake = fake();
    let candidates = fake.admitted(&["a", "b"]);
    assert_eq!(pick(&AtSlotZero(&fake), &candidates), "a");
}

#[test]
fn with_key_1_constant_the_late_peak_is_not_seen_and_a_is_picked() {
    // The fit of a flat lease is the residual now (`L = 1`); only key 1 looks at minute 10.
    let fake = fake();
    let candidates = fake.admitted(&["a", "b"]);
    assert_eq!(pick_with(&mut key_1_constant(), &fake, &candidates), "a");
}
