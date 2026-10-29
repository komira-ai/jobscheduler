//! One reading of the machine's actions cgroup and of each lease's cgroup.

use crate::facts::FileReader;

pub use js_agent_usage::{LeaseSample, Sample};

use super::cgroup::{
    cpu_limited, parse_active_file, parse_inactive_file, parse_single_u64, parse_usage_usec,
    working_set,
};
use super::clock::UsageClock;

/// A lease whose cgroup is to be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseCgroup {
    /// The lease's id as the server knows it.
    pub lease_id: String,
    /// The lease's cgroup directory.
    pub dir: String,
    /// When the lease started, on the [`UsageClock`] that samples are taken with.
    pub started_usec: u64,
}

/// Why a [`Sample`] could not be taken. The text is fixed: no path, no file content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SampleError {
    /// The actions cgroup's `cpu.stat` is unreadable or has no `usage_usec`.
    #[error("the node cpu.stat cannot be read")]
    NodeCpuStat,
    /// The actions cgroup's `memory.current` is unreadable or not a number.
    #[error("the node memory.current cannot be read")]
    NodeMemoryCurrent,
    /// The actions cgroup's `memory.stat` is unreadable or lacks a file cache key.
    #[error("the node memory.stat cannot be read")]
    NodeMemoryStat,
}

/// Reads the cgroup tree under the actions directory through a [`FileReader`].
pub struct Sampler<F: FileReader> {
    files: F,
    actions_dir: String,
}

impl<F: FileReader> Sampler<F> {
    /// A sampler of the cgroup at `actions_dir`, the parent of every lease's cgroup.
    pub fn new(files: F, actions_dir: impl Into<String>) -> Self {
        Self {
            files,
            actions_dir: actions_dir.into(),
        }
    }

    /// Reads the node and then each of `leases`, with exactly one read of `clock`.
    ///
    /// A node file that cannot be read is an error, so that no zero is invented. A lease
    /// that cannot be read is left out and counted in `unreadable_leases`.
    pub fn sample(
        &self,
        clock: &dyn UsageClock,
        leases: &[LeaseCgroup],
    ) -> Result<Sample, SampleError> {
        let dir = self.actions_dir.as_str();
        let cpu = self
            .value(dir, "cpu.stat", parse_usage_usec)
            .ok_or(SampleError::NodeCpuStat)?;
        let mono_usec = clock.now_usec();
        let current = self
            .value(dir, "memory.current", parse_single_u64)
            .ok_or(SampleError::NodeMemoryCurrent)?;
        let inactive = self
            .value(dir, "memory.stat", parse_inactive_file)
            .ok_or(SampleError::NodeMemoryStat)?;
        let active = self
            .value(dir, "memory.stat", parse_active_file)
            .ok_or(SampleError::NodeMemoryStat)?;
        let limited = self
            .files
            .read(&format!("{dir}/cpu.max"))
            .ok()
            .and_then(|text| cpu_limited(&text))
            .unwrap_or(true);
        let read: Vec<Option<LeaseSample>> = leases.iter().map(|l| self.lease(l)).collect();
        let unreadable = read.iter().filter(|lease| lease.is_none()).count();
        Ok(Sample {
            mono_usec,
            cpu_usage_usec: cpu,
            working_set_bytes: working_set(current, inactive),
            active_file_bytes: active,
            cpu_limited: limited,
            leases: read.into_iter().flatten().collect(),
            unreadable_leases: u32::try_from(unreadable).unwrap_or(u32::MAX),
        })
    }

    /// The number `parse` finds in the file `name` of `dir`.
    fn value(&self, dir: &str, name: &str, parse: fn(&str) -> Option<u64>) -> Option<u64> {
        parse(&self.files.read(&format!("{dir}/{name}")).ok()?)
    }

    fn lease(&self, lease: &LeaseCgroup) -> Option<LeaseSample> {
        let dir = lease.dir.as_str();
        let cpu = self.value(dir, "cpu.stat", parse_usage_usec)?;
        let current = self.value(dir, "memory.current", parse_single_u64)?;
        let inactive = self.value(dir, "memory.stat", parse_inactive_file)?;
        Some(LeaseSample {
            lease_id: lease.lease_id.clone(),
            started_usec: lease.started_usec,
            cpu_usage_usec: cpu,
            working_set_bytes: working_set(current, inactive),
        })
    }
}
