//! The plan: a schedule drawn from the seed alone that runs every arm the asked number
//! of times.

use std::collections::BTreeMap;

use js_measure::plan::draws;
use js_measure::{Plan, PlanError, Slot};

use crate::support::{plan, plan_over};

fn arms_of(slots: &[Slot]) -> Vec<usize> {
    slots.iter().map(|slot| slot.arm).collect()
}

/// Asserts that the same plan and seed give the same schedule, that the schedule is the
/// one the reference in `tests/reference/schedule.py` (SplitMix64 and Fisher-Yates, run
/// and checked there) gives for a pinned seed, so it is the same on every machine, and
/// that another seed gives another order.
#[test]
fn the_order_is_reproducible_from_the_seed() {
    let one = plan(&["a", "b", "c"], 2, 1, 42);
    let first = one.schedule().unwrap();
    assert_eq!(first, one.schedule().unwrap());
    assert_eq!(arms_of(&first), vec![0, 2, 1, 1, 0, 2, 1, 2, 0]);
    let again = plan(&["a", "b", "c"], 2, 0, 7).schedule().unwrap();
    assert_eq!(arms_of(&again), vec![0, 2, 0, 1, 2, 1]);
    let other = plan(&["a", "b", "c"], 2, 1, 43).schedule().unwrap();
    assert_ne!(arms_of(&first), arms_of(&other));
}

/// Asserts that every arm runs exactly `runs_per_arm` times measured and
/// `warmups_per_arm` times warming up, the warm-ups first, and that the orders count
/// 0, 1, 2, ... without a gap. It catches an arm that is dropped or doubled.
#[test]
fn every_arm_runs_n_times_and_warmups_come_first() {
    let slots = plan(&["a", "b", "c", "d", "e"], 3, 2, 9)
        .schedule()
        .unwrap();
    assert_eq!(slots.len(), 5 * (3 + 2));
    let orders: Vec<u32> = slots.iter().map(|slot| slot.order).collect();
    assert_eq!(orders, (0..25).collect::<Vec<u32>>());
    let warmups = slots.iter().take_while(|slot| slot.warmup).count();
    assert_eq!(warmups, 10);
    assert!(slots[warmups..].iter().all(|slot| !slot.warmup));
    let mut measured: BTreeMap<usize, u32> = BTreeMap::new();
    let mut warm: BTreeMap<usize, u32> = BTreeMap::new();
    for slot in &slots {
        let counts = if slot.warmup {
            &mut warm
        } else {
            &mut measured
        };
        *counts.entry(slot.arm).or_default() += 1;
    }
    assert_eq!(measured, (0..5).map(|arm| (arm, 3)).collect());
    assert_eq!(warm, (0..5).map(|arm| (arm, 2)).collect());
}

/// Asserts that the runs of an arm are not kept together: four arms of five runs in one
/// block per arm is one order in a hundred thousand million, so any seed that gives it
/// is a draw that does not shuffle.
#[test]
fn the_order_is_shuffled_not_grouped() {
    let slots = plan(&["a", "b", "c", "d"], 5, 0, 1).schedule().unwrap();
    let grouped: Vec<usize> = (0..4).flat_map(|arm| [arm; 5]).collect();
    assert_ne!(arms_of(&slots), grouped);
    let sorted = {
        let mut arms = arms_of(&slots);
        arms.sort_unstable();
        arms
    };
    assert_eq!(sorted, grouped);
}

/// Asserts the three ways a plan is refused: no arm, no run, a repeated label.
#[test]
fn a_bad_plan_is_refused() {
    assert_eq!(plan(&[], 3, 0, 1).schedule(), Err(PlanError::NoArms));
    assert_eq!(plan(&["a"], 0, 0, 1).validate(), Err(PlanError::NoRuns));
    assert_eq!(
        plan(&["a", "b", "a"], 1, 0, 1).validate(),
        Err(PlanError::DuplicateArm("a".to_owned()))
    );
    assert_eq!(
        plan_over(&[], &["a"], 1, 0, 1).validate(),
        Err(PlanError::NoSettings)
    );
    assert_eq!(
        plan_over(&["1", "2", "1"], &["a"], 1, 0, 1).validate(),
        Err(PlanError::DuplicateSetting("1".to_owned()))
    );
    assert_eq!(plan(&["a", "b"], 1, 0, 1).validate(), Ok(()));
}

/// Asserts how a plan file is read: the control flag and the warm-ups are optional, and
/// a key the plan does not know is refused (a misspelt `runs_per_arm` must not run once).
#[test]
fn a_plan_file_is_read_strictly() {
    let text = r#"{"parameter":"fill","settings":["0.8","0.9"],"arms":[{"label":"whole"},{"label":"booked","control":true}],"runs_per_arm":3,"seed":5}"#;
    let read: Plan = serde_json::from_str(text).unwrap();
    assert!(!read.arms[0].control);
    assert!(read.arms[1].control);
    assert_eq!(read.warmups_per_arm, 0);
    let bad = r#"{"parameter":"fill","settings":[],"arms":[],"runs_per_arm":3,"seed":5,"runs":3}"#;
    assert!(serde_json::from_str::<Plan>(bad).is_err());
}

/// Asserts the cells: the plan runs every setting with every arm, each cell the asked
/// number of times, in an order pinned for a seed (so it is the same on every machine),
/// and the slot names the setting and the arm of its cell.
#[test]
fn every_setting_and_arm_pair_is_a_cell_run_n_times() {
    let slots = plan_over(&["s0", "s1"], &["a", "b"], 2, 0, 7)
        .schedule()
        .unwrap();
    let cells: Vec<(usize, usize)> = slots.iter().map(|slot| (slot.setting, slot.arm)).collect();
    assert_eq!(
        cells,
        vec![
            (0, 0),
            (1, 0),
            (1, 0),
            (0, 1),
            (1, 1),
            (0, 0),
            (0, 1),
            (1, 1)
        ]
    );
    let mut counts: BTreeMap<(usize, usize), u32> = BTreeMap::new();
    for cell in cells {
        *counts.entry(cell).or_default() += 1;
    }
    assert_eq!(counts.len(), 4);
    assert!(counts.values().all(|n| *n == 2));
}

/// Asserts that the generator is SplitMix64: its first draws for seed 1234567 are the
/// published test vector (the same five numbers as in the rand_xoshiro crate's reference
/// test), which `tests/reference/schedule.py` also checks. It catches a changed constant or
/// shift that would still give a reproducible, but different, order.
#[test]
fn the_generator_is_splitmix64() {
    assert_eq!(
        draws(1_234_567, 5),
        vec![
            6_457_827_717_110_365_317,
            3_203_168_211_198_807_973,
            9_817_491_932_198_370_423,
            4_593_380_528_125_082_431,
            16_408_922_859_458_223_821,
        ]
    );
}
