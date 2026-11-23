//! The non-file memory of a lease's group ([`nonfile_of`]), which the crun runtime samples at
//! each poll into the run's [`MemoryRecorder`](crate::series::memory::MemoryRecorder): what the
//! job's result reports as
//! [`JobResult::memory_series`](js_agent_wire::JobResult::memory_series).
//!
//! Non-file memory is `memory.current - (file - shmem)` of the lease's group: anonymous and
//! kernel memory and shared memory and tmpfs pages, which the kernel cannot drop, and none of
//! the page cache of the files the job read, which it can.

use std::path::Path;

use crate::usage::{parse_single_u64, parse_stat_key};

/// The non-file memory of the group at `dir`, as read now: `memory.current` less the file
/// pages of its `memory.stat` that are not shared memory (`file - shmem`), never below 0.
/// `None` when either file cannot be read or lacks the figure.
pub(super) fn nonfile_of(dir: &Path) -> Option<u64> {
    let current = parse_single_u64(&std::fs::read_to_string(dir.join("memory.current")).ok()?)?;
    nonfile(
        current,
        &std::fs::read_to_string(dir.join("memory.stat")).ok()?,
    )
}

/// `current` less `file - shmem` of the `memory.stat` text `stat`, never below 0. `None`
/// when the text lacks `file` or `shmem`.
fn nonfile(current: u64, stat: &str) -> Option<u64> {
    let file = parse_stat_key(stat, "file")?;
    let shmem = parse_stat_key(stat, "shmem")?;
    Some(current.saturating_sub(file.saturating_sub(shmem)))
}

#[cfg(test)]
#[path = "memory_series_tests.rs"]
mod tests;
