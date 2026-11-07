//! Values the tests share.

use js_measure::{Arm, Job, JobAsk, Plan};

/// A gibibyte.
pub const GIB: u64 = 1 << 30;

/// The fake clock's start: the beginning of minute 10 of the usage store.
pub const T0: u64 = 600_000;

/// A plan at one setting (`1.0`) over the arms named, none of them the control.
pub fn plan(arms: &[&str], runs_per_arm: u32, warmups_per_arm: u32, seed: u64) -> Plan {
    plan_over(&["1.0"], arms, runs_per_arm, warmups_per_arm, seed)
}

/// A plan over the settings and the arms named, none of the arms the control.
pub fn plan_over(
    settings: &[&str],
    arms: &[&str],
    runs_per_arm: u32,
    warmups_per_arm: u32,
    seed: u64,
) -> Plan {
    Plan {
        parameter: "fill".to_owned(),
        settings: settings.iter().map(|s| (*s).to_owned()).collect(),
        arms: arms
            .iter()
            .map(|label| Arm {
                label: (*label).to_owned(),
                control: false,
                whole_host: None,
            })
            .collect(),
        runs_per_arm,
        warmups_per_arm,
        seed,
    }
}

/// A job of 4 cores and 8 GiB, sharing a host, that runs for `wall_ms` (rounded up to
/// whole seconds), submitted at the start.
pub fn job(name: &str, wall_ms: u64) -> Job {
    Job {
        name: name.to_owned(),
        ask: JobAsk {
            image: "img".to_owned(),
            args: Vec::new(),
            cpu_millis: 4_000,
            memory_bytes: 8 * GIB,
            whole_host: false,
            wall_limit_seconds: wall_ms.div_ceil(1000),
        },
        submit_at_ms: 0,
    }
}
