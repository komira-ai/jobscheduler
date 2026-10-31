//! Values the tests share.

use js_measure::{Arm, Job, LeaseAsk, Plan, RunAsk};

/// A gibibyte.
pub const GIB: u64 = 1 << 30;

/// The fake clock's start: the beginning of minute 10 of the usage store.
pub const T0: u64 = 600_000;

/// A plan over the arms named, none of them the control.
pub fn plan(arms: &[&str], runs_per_arm: u32, warmups_per_arm: u32, seed: u64) -> Plan {
    Plan {
        setting: "fill".to_owned(),
        arms: arms
            .iter()
            .map(|label| Arm {
                label: (*label).to_owned(),
                control: false,
            })
            .collect(),
        runs_per_arm,
        warmups_per_arm,
        seed,
    }
}

/// A job of 4 cores and 8 GiB that runs for `wall_ms`, submitted at the start.
pub fn job(name: &str, wall_ms: u64, profile: Option<&str>) -> Job {
    Job {
        name: name.to_owned(),
        lease: LeaseAsk {
            class: "whole-node".to_owned(),
            cpu_millis: 4_000,
            memory_bytes: 8 * GIB,
            max_wall_seconds: 3_600,
        },
        run: RunAsk {
            image: "img".to_owned(),
            args: Vec::new(),
            cpu_millis: 4_000,
            memory_bytes: 8 * GIB,
            wall_ms,
            profile: profile.map(str::to_owned),
        },
        submit_at_ms: 0,
    }
}
