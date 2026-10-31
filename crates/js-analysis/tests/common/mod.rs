//! Builders shared by the tests.

use js_analysis::RunRecord;

/// A record with the named metrics (`None` is not available).
pub fn rec(
    setting: &str,
    arm: &str,
    run_index: u32,
    label: &str,
    metrics: &[(&str, Option<f64>)],
) -> RunRecord {
    RunRecord {
        setting: setting.to_string(),
        arm: arm.to_string(),
        run_index,
        seed: u64::from(run_index) + 100,
        commit: "c0ffee1".to_string(),
        label: label.to_string(),
        metrics: metrics
            .iter()
            .map(|(name, v)| ((*name).to_string(), *v))
            .collect(),
    }
}

/// Measured runs of one arm with one metric `m`, indexed from 0.
pub fn arm_runs(setting: &str, arm: &str, values: &[f64]) -> Vec<RunRecord> {
    values
        .iter()
        .enumerate()
        .map(|(i, v)| rec(setting, arm, i as u32, "", &[("m", Some(*v))]))
        .collect()
}
