//! Runs a plan: for each slot of the schedule, puts the arm's setting in place, drives
//! the workload and hands the measured run to the caller to write.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::process::{Command, Stdio};

use crate::clock::Clock;
use crate::driver::drive;
use crate::farm::Farm;
use crate::measure::{HostReading, Identity, MINUTE_MS, Prediction, RunOutput, assemble};
use crate::metric::WARMUP;
use crate::plan::{Arm, Plan, PlanError};
use crate::workload::Job;

/// Why a setter could not put an arm's setting in place.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SetError {
    /// The command could not be started.
    #[error("cannot start {0:?}: {1}")]
    Spawn(String, String),
    /// The command ended with a failure.
    #[error("{0:?} failed: {1}")]
    Failed(String, String),
}

/// Why a plan stopped.
#[derive(Debug, thiserror::Error)]
pub enum ExperimentError {
    /// The plan cannot be run.
    #[error(transparent)]
    Plan(#[from] PlanError),
    /// An arm's setting could not be put in place.
    #[error(transparent)]
    Set(#[from] SetError),
    /// The caller could not write a run's output.
    #[error("cannot write the records: {0}")]
    Emit(#[from] io::Error),
}

/// Puts the setting of an arm in place before its run. Between a setter's success and the
/// run, nothing else changes the setting.
pub trait Setter {
    /// Puts `arm` in place at `setting` of `parameter`.
    ///
    /// # Errors
    /// [`SetError`] when it cannot.
    fn apply(&self, parameter: &str, setting: &str, arm: &Arm) -> Result<(), SetError>;
}

/// Leaves the setting alone: the operator or the dry run owns it.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoSetter;

impl Setter for NoSetter {
    fn apply(&self, _parameter: &str, _setting: &str, _arm: &Arm) -> Result<(), SetError> {
        Ok(())
    }
}

/// Runs a program as `<program> <parameter> <setting> <arm label> control|treatment`, stdin closed,
/// and counts a nonzero exit as a failure.
#[derive(Debug, Clone)]
pub struct CommandSetter {
    /// The program to run.
    pub program: String,
}

impl Setter for CommandSetter {
    fn apply(&self, parameter: &str, setting: &str, arm: &Arm) -> Result<(), SetError> {
        let kind = if arm.control { "control" } else { "treatment" };
        let status = Command::new(&self.program)
            .args([parameter, setting, arm.label.as_str(), kind])
            .stdin(Stdio::null())
            .status()
            .map_err(|err| SetError::Spawn(self.program.clone(), err.to_string()))?;
        match status.success() {
            true => Ok(()),
            false => Err(SetError::Failed(self.program.clone(), status.to_string())),
        }
    }
}

/// Everything a plan needs to run.
pub struct Experiment<'a> {
    /// The plan.
    pub plan: &'a Plan,
    /// The workload, expanded.
    pub jobs: &'a [Job],
    /// The farm.
    pub farm: &'a dyn Farm,
    /// The clock.
    pub clock: &'a dyn Clock,
    /// Puts each arm's setting in place.
    pub setter: &'a dyn Setter,
    /// The commit under test.
    pub commit: &'a str,
    /// Milliseconds between polls.
    pub poll_ms: u64,
    /// Milliseconds after which a run's open jobs are given up.
    pub deadline_ms: u64,
}

impl Experiment<'_> {
    /// One run of the workload under `who`.
    fn once(&self, who: &Identity) -> RunOutput {
        let mut predictions = BTreeMap::new();
        for profile in self.jobs.iter().filter_map(|job| job.run.profile.as_ref()) {
            let curve = self.farm.curve(profile).ok().flatten();
            if let Some(prediction) = curve.as_ref().and_then(Prediction::of) {
                predictions.insert(profile.clone(), prediction);
            }
        }
        let obs = drive(
            self.farm,
            self.clock,
            self.jobs,
            self.poll_ms,
            self.deadline_ms,
        );
        let used: BTreeSet<&String> = obs
            .jobs
            .iter()
            .filter_map(|job| job.host.as_ref())
            .collect();
        // The minutes of the usage store are the server's; these are this clock's, assumed
        // to be in sync with it (see `Clock::now_ms`).
        let (from, to) = (obs.t0_ms / MINUTE_MS, obs.end_ms / MINUTE_MS);
        let hosts = used
            .into_iter()
            .map(|host| {
                let reading = HostReading {
                    shape: self.farm.host(host).ok(),
                    rows: self.farm.usage_rows(host, from, to).ok(),
                };
                (host.clone(), reading)
            })
            .collect();
        assemble(who, &obs, self.jobs, &predictions, &hosts)
    }
}

/// Runs every slot of the plan's schedule in order and hands each run's output to `emit`
/// as soon as it is measured, so a sweep that dies keeps what it had. Returns the number
/// of runs.
///
/// # Errors
///
/// [`ExperimentError`] when the plan is invalid, a setter fails or `emit` does; the runs
/// before it were emitted.
pub fn run_plan(
    exp: &Experiment<'_>,
    emit: &mut dyn FnMut(&RunOutput) -> io::Result<()>,
) -> Result<usize, ExperimentError> {
    let slots = exp.plan.schedule()?;
    for slot in &slots {
        let arm = &exp.plan.arms[slot.arm];
        let setting = &exp.plan.settings[slot.setting];
        exp.setter.apply(&exp.plan.parameter, setting, arm)?;
        let who = Identity {
            setting: setting.clone(),
            arm: arm.label.clone(),
            run_index: slot.order,
            seed: exp.plan.seed,
            commit: exp.commit.to_owned(),
            label: if slot.warmup {
                WARMUP.to_owned()
            } else {
                String::new()
            },
        };
        emit(&exp.once(&who))?;
    }
    Ok(slots.len())
}
