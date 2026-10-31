//! A synthetic job mix, defined as data: kinds of job, how many of each, what they ask
//! for and when they are submitted.

use serde::{Deserialize, Serialize};

use crate::farm::{LeaseAsk, RunAsk};

/// Why a workload cannot be used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WorkloadError {
    /// The workload expands to no job.
    #[error("the workload has no jobs")]
    Empty,
}

fn whole_node() -> String {
    "whole-node".to_owned()
}

/// A kind of job, repeated `count` times.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobKind {
    /// The kind's name; its jobs are `<name>-0`, `<name>-1`, ...
    pub name: String,
    /// How many jobs of the kind.
    pub count: u32,
    /// The container image of the run.
    pub image: String,
    /// The image's arguments.
    #[serde(default)]
    pub args: Vec<String>,
    /// CPU booked by the lease and limited on the run, in thousandths of a core.
    pub cpu_millis: u64,
    /// Memory booked and limited, in bytes.
    pub memory_bytes: u64,
    /// The run's wall limit, in milliseconds.
    pub wall_ms: u64,
    /// The lease class; `whole-node` when left out.
    #[serde(default = "whole_node")]
    pub class: String,
    /// The profile the server learns this kind's cost curve under.
    #[serde(default)]
    pub profile: Option<String>,
    /// When the first job of the kind is submitted, in milliseconds after the run begins.
    #[serde(default)]
    pub submit_at_ms: u64,
    /// The gap between the submissions of two jobs of the kind.
    #[serde(default)]
    pub stagger_ms: u64,
}

/// The job mix as a file holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Workload {
    /// The kinds.
    pub jobs: Vec<JobKind>,
}

/// One job to submit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    /// `<kind>-<index>`.
    pub name: String,
    /// The lease to ask for.
    pub lease: LeaseAsk,
    /// The run to queue on it.
    pub run: RunAsk,
    /// Milliseconds after the run begins at which the job is submitted.
    pub submit_at_ms: u64,
}

impl Workload {
    /// The jobs, by submission time (kinds in file order among equal times).
    ///
    /// # Errors
    ///
    /// [`WorkloadError::Empty`] when the workload has no job.
    pub fn expand(&self) -> Result<Vec<Job>, WorkloadError> {
        let mut jobs: Vec<Job> = self
            .jobs
            .iter()
            .flat_map(|kind| {
                (0..kind.count).map(move |index| Job {
                    name: format!("{}-{index}", kind.name),
                    lease: LeaseAsk {
                        class: kind.class.clone(),
                        cpu_millis: kind.cpu_millis,
                        memory_bytes: kind.memory_bytes,
                        max_wall_seconds: kind.wall_ms.div_ceil(1000) + 60,
                    },
                    run: RunAsk {
                        image: kind.image.clone(),
                        args: kind.args.clone(),
                        cpu_millis: kind.cpu_millis,
                        memory_bytes: kind.memory_bytes,
                        wall_ms: kind.wall_ms,
                        profile: kind.profile.clone(),
                    },
                    submit_at_ms: kind.submit_at_ms + u64::from(index) * kind.stagger_ms,
                })
            })
            .collect();
        if jobs.is_empty() {
            return Err(WorkloadError::Empty);
        }
        jobs.sort_by_key(|job| job.submit_at_ms);
        Ok(jobs)
    }
}
