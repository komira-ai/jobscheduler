//! The run plan: the parameter under test, the values it takes (the settings), the arms
//! compared at each setting, how often each cell runs and the order
//! they run in. The order is drawn from the seed, so the same plan and seed give the
//! same schedule on any machine.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// Why a plan cannot be run.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlanError {
    /// The plan has no arm.
    #[error("the plan has no arms")]
    NoArms,
    /// The plan has no setting.
    #[error("the plan has no settings")]
    NoSettings,
    /// Two settings are the same text, so their runs could not be told apart.
    #[error("two settings are both {0:?}")]
    DuplicateSetting(String),
    /// Two arms share a label, so their runs could not be told apart.
    #[error("two arms are both called {0:?}")]
    DuplicateArm(String),
    /// An arm would never run.
    #[error("runs_per_arm must be at least 1")]
    NoRuns,
}

/// One arm: what is run at each setting (for example a placement rule), or the control.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Arm {
    /// The arm's name in the records, for example `0.8` or `booked-rule`.
    pub label: String,
    /// Whether the arm is the control (the old behaviour). The setter is told.
    #[serde(default)]
    pub control: bool,
}

/// The plan as a file holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    /// The parameter that varies, for example `fill`; the setter is told its name.
    pub parameter: String,
    /// The values it takes, as the text the records carry in `setting` (for example `0.8`).
    pub settings: Vec<String>,
    /// The arms, run at every setting.
    pub arms: Vec<Arm>,
    /// Measured runs of every cell (every setting with every arm).
    pub runs_per_arm: u32,
    /// Warm-up runs of every cell, run before the measured ones and labelled in the records.
    #[serde(default)]
    pub warmups_per_arm: u32,
    /// The seed of the order.
    pub seed: u64,
}

/// One run of the schedule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slot {
    /// Its place in the executed order, from 0; this is the record's `run_index`.
    pub order: u32,
    /// The index of its setting in [`Plan::settings`].
    pub setting: usize,
    /// The index of its arm in [`Plan::arms`].
    pub arm: usize,
    /// Whether it is a warm-up.
    pub warmup: bool,
}

/// SplitMix64: a small generator whose stream is fixed by its seed alone.
struct Rng(u64);

impl Rng {
    fn draw(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Fisher-Yates. The modulo bias is at most 2^-50 for a schedule of a few hundred.
    fn shuffle(&mut self, items: &mut [usize]) {
        for at in (1..items.len()).rev() {
            let with = usize::try_from(self.draw() % (at as u64 + 1)).unwrap_or(0);
            items.swap(at, with);
        }
    }
}

/// The first `count` raw draws of the generator for `seed`, so that a test can pin them to
/// published SplitMix64 vectors (see `the_generator_is_splitmix64` for their sources).
#[doc(hidden)]
#[must_use]
pub fn draws(seed: u64, count: usize) -> Vec<u64> {
    let mut rng = Rng(seed);
    (0..count).map(|_| rng.draw()).collect()
}

impl Plan {
    /// Checks the plan.
    ///
    /// # Errors
    ///
    /// [`PlanError`] when there is no arm, a label repeats, or no run is asked for.
    pub fn validate(&self) -> Result<(), PlanError> {
        if self.arms.is_empty() {
            return Err(PlanError::NoArms);
        }
        if self.settings.is_empty() {
            return Err(PlanError::NoSettings);
        }
        let mut values = BTreeSet::new();
        for setting in &self.settings {
            if !values.insert(setting.as_str()) {
                return Err(PlanError::DuplicateSetting(setting.clone()));
            }
        }
        if self.runs_per_arm == 0 {
            return Err(PlanError::NoRuns);
        }
        let mut seen = BTreeSet::new();
        for arm in &self.arms {
            if !seen.insert(arm.label.as_str()) {
                return Err(PlanError::DuplicateArm(arm.label.clone()));
            }
        }
        Ok(())
    }

    /// The runs in the order they are executed: every warm-up first, then every measured
    /// run, each group in an order drawn from the seed. Every arm appears exactly
    /// `warmups_per_arm` times among the first group and `runs_per_arm` times in the
    /// second.
    ///
    /// # Errors
    ///
    /// [`PlanError`] from [`Plan::validate`].
    pub fn schedule(&self) -> Result<Vec<Slot>, PlanError> {
        self.validate()?;
        let mut rng = Rng(self.seed);
        let mut slots = Vec::new();
        for (count, warmup) in [(self.warmups_per_arm, true), (self.runs_per_arm, false)] {
            // A cell is a setting and an arm, numbered setting-major.
            let cells = self.settings.len() * self.arms.len();
            let mut runs: Vec<usize> = (0..cells)
                .flat_map(|cell| (0..count).map(move |_| cell))
                .collect();
            rng.shuffle(&mut runs);
            slots.extend(runs.into_iter().map(|cell| Slot {
                order: 0,
                setting: cell / self.arms.len(),
                arm: cell % self.arms.len(),
                warmup,
            }));
        }
        for (order, slot) in (0..).zip(slots.iter_mut()) {
            slot.order = order;
        }
        Ok(slots)
    }
}
