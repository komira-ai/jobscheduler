//! Values the tests share.

use js_measure::{Arm, Job, LeaseAsk, Plan, RunAsk};

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

/// The record as the analysis crate defines it (`RunRecord`, read with
/// `deny_unknown_fields`), copied field for field so that a line this crate emits is
/// checked against the reader's own shape without depending on the reader.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnalysisRunRecord {
    pub setting: String,
    pub arm: String,
    pub run_index: u32,
    pub seed: u64,
    pub commit: String,
    #[serde(default)]
    pub label: String,
    pub metrics: std::collections::BTreeMap<String, Option<f64>>,
}
