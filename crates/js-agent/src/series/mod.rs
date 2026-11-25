//! The series a job's result carries, second by second over its run: its CPU
//! ([`cpu::SeriesRecorder`], `--cpu-series`) and its memory ([`memory::MemoryRecorder`],
//! `--memory-series`). Both are pure: the runtime reads its clock and its run's counters at
//! each poll and gives them the samples, the crun runtime from the lease's group and the
//! native runtime of a Mac from the run's processes ([`crate::mac_usage`]).

pub(crate) mod cpu;
pub(crate) mod memory;
