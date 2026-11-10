//! [`Invariant::UsageRowPerEndedAllocation`] and [`Invariant::UsageTotalsMatchTerminalJobs`]:
//! the usage history the server keeps agrees with the allocations it freed.
//!
//! Both are end checks over [`Observations::usage`], the rows the cell read from the
//! server's usage route once the scenario had settled, and the allocation rows it read
//! over the run. Both are skipped when the cell read no usage history (`None`): a server
//! that was down at the end, or a hand-built history with no usage in it.
//!
//! An ended allocation is one the cell last read in the step `Freed`. The server writes its
//! row in the same transaction as the free, named by the job and the time of the free, so a
//! row belongs to an ended allocation when it names the allocation's job and host and ends
//! at the allocation's `freed_at`. A job can have several allocations (a failed whole-host
//! prepare frees one and places the job again), each with its own row. The store keeps one
//! allocation row per job, so an allocation the cell saw replaced by the job's next one is
//! read freed with no `freed_at` (`crate::jobs::read`): it was freed at some time between its
//! creation and that read, and its row is the one row of its job and host that ended in that
//! span and that no allocation with a known free time owns.
//!
//! # `UsageRowPerEndedAllocation`
//!
//! Each ended allocation has exactly one row: none is a lost row, two are a doubled one.
//! No row exists for an allocation that has not ended: a row that belongs to no ended
//! allocation is flagged unless it ended no later than the last allocation the cell read of
//! its job was created. The cell reads one allocation row per job, the latest, so a
//! placement the job left for a new one between two reads is not seen freed; its row is
//! excused, and a row for an allocation the cell never saw at all is not.
//!
//! # `UsageTotalsMatchTerminalJobs`
//!
//! For each job whose rows and ended allocations are the same in number (a job whose rows
//! are lost or doubled is the first invariant's), the totals of its rows equal the totals of
//! its ended allocations: the number, the CPU and the memory booked (a whole host books
//! neither, as the row shows it: [`crate::usage::booked_totals`]) and the set of hosts.
//! The queue wait and the run time of a row are not compared: they come from the job's
//! timestamps, which the cell does not read.

use std::collections::{BTreeMap, BTreeSet};

use js_jobs::{AllocationStep, JobId};
use js_sim::text::time;
use js_types::HostId;
use js_types::sched::FarmTime;

use super::{Found, Invariant};
use crate::jobs::{AllocationRead, AllocationRow};
use crate::observe::Observations;
use crate::text;
use crate::usage::{UsageRow, booked_totals};

/// Where a usage row says an allocation ended: the job, the host and the time of the free.
type Key<'a> = (&'a JobId, &'a HostId, FarmTime);

/// What the usage rows, or the ended allocations, of one job add up to.
#[derive(Default, PartialEq, Eq)]
struct Totals {
    count: usize,
    cpu_millis: u128,
    memory_bytes: u128,
    hosts: BTreeSet<HostId>,
}

impl Totals {
    /// These totals with one more booking of `cpu_millis` and `memory_bytes` on `host`.
    fn add(&mut self, host: &HostId, cpu_millis: u64, memory_bytes: u64) {
        self.count += 1;
        self.cpu_millis += u128::from(cpu_millis);
        self.memory_bytes += u128::from(memory_bytes);
        self.hosts.insert(host.clone());
    }

    /// The totals as a violation writes them.
    fn text(&self) -> String {
        let hosts: Vec<&str> = self.hosts.iter().map(HostId::as_str).collect();
        format!(
            "{} allocations, cpu {} millis, memory {} bytes, on [{}]",
            self.count,
            self.cpu_millis,
            self.memory_bytes,
            hosts.join(", ")
        )
    }
}

/// The ended allocations the cell read: the last read of each whose row is `Freed`.
fn ended_allocations(observations: &Observations) -> Vec<&AllocationRead> {
    let mut last = BTreeMap::new();
    for read in &observations.allocation_reads {
        last.insert(read.row.allocation, read);
    }
    last.into_values()
        .filter(|read| read.row.step == AllocationStep::Freed)
        .collect()
}

/// `UsageRowPerEndedAllocation` and `UsageTotalsMatchTerminalJobs` over `observations`.
pub(super) fn check(observations: &Observations, found: &mut Found) {
    let Some(rows) = &observations.usage else {
        return;
    };
    let ended = ended_allocations(observations);
    rows_per_allocation(observations, &ended, rows, found);
    let ended: Vec<&AllocationRow> = ended.iter().map(|read| &read.row).collect();
    totals(&ended, rows, found);
}

/// `UsageRowPerEndedAllocation`.
fn rows_per_allocation(
    observations: &Observations,
    ended: &[&AllocationRead],
    rows: &[UsageRow],
    found: &mut Found,
) {
    let mut named: BTreeMap<Key<'_>, usize> = BTreeMap::new();
    for row in rows {
        *named
            .entry((&row.job, &row.host, row.ended_at))
            .or_default() += 1;
    }
    let mut owned: BTreeSet<Key<'_>> = BTreeSet::new();
    let (stamped, unstamped): (Vec<&&AllocationRead>, Vec<&&AllocationRead>) =
        ended.iter().partition(|read| read.row.freed_at.is_some());
    for read in stamped {
        let allocation = &read.row;
        let at = allocation.freed_at.unwrap_or(allocation.created_at);
        let key = (&allocation.job, &allocation.host, at);
        owned.insert(key);
        let count = named.get(&key).copied().unwrap_or_default();
        if count != 1 {
            let detail = format!(
                "job {} freed on host {} at {} has {count} usage rows, not one",
                allocation.job,
                text::host(&allocation.host),
                time(at)
            );
            found.flag(Invariant::UsageRowPerEndedAllocation, detail);
        }
    }
    for read in unstamped {
        let allocation = &read.row;
        let span = allocation.created_at..=read.at;
        let candidates: Vec<(&Key<'_>, &usize)> = named
            .iter()
            .filter(|(key, _)| (key.0 == &allocation.job) & (key.1 == &allocation.host))
            .filter(|(key, _)| span.contains(&key.2) & !owned.contains(*key))
            .collect();
        let count: usize = candidates.iter().map(|(_, count)| **count).sum();
        owned.extend(candidates.iter().map(|(key, _)| **key));
        if count != 1 {
            let detail = format!(
                "job {} freed on host {} between {} and {} has {count} usage rows, not one",
                allocation.job,
                text::host(&allocation.host),
                time(allocation.created_at),
                time(read.at)
            );
            found.flag(Invariant::UsageRowPerEndedAllocation, detail);
        }
    }
    let latest = latest_creation(observations);
    for (key, _) in named.iter().filter(|(key, _)| !owned.contains(*key)) {
        let (job, host, at) = *key;
        let excused = latest.get(job).is_some_and(|created| at <= *created);
        if !excused {
            let detail = format!(
                "a usage row says job {job} ended on host {} at {}, and no allocation of it was read freed then",
                text::host(host),
                time(at)
            );
            found.flag(Invariant::UsageRowPerEndedAllocation, detail);
        }
    }
}

/// When the last allocation the cell read of each job was created.
fn latest_creation(observations: &Observations) -> BTreeMap<&JobId, FarmTime> {
    let mut latest = BTreeMap::new();
    for read in &observations.allocation_reads {
        let created = latest.entry(&read.row.job).or_insert(read.row.created_at);
        *created = (*created).max(read.row.created_at);
    }
    latest
}

/// `UsageTotalsMatchTerminalJobs`.
fn totals(ended: &[&AllocationRow], rows: &[UsageRow], found: &mut Found) {
    let mut jobs: BTreeMap<&JobId, (Totals, Totals)> = BTreeMap::new();
    for allocation in ended {
        let (cpu, memory) = booked_totals(allocation.booked);
        let (_, own) = jobs.entry(&allocation.job).or_default();
        own.add(&allocation.host, cpu, memory);
    }
    for row in rows {
        let (written, _) = jobs.entry(&row.job).or_default();
        written.add(&row.host, row.cpu_millis, row.memory_bytes);
    }
    for (job, (written, own)) in jobs {
        if (written.count == own.count) & (written != own) {
            let detail = format!(
                "the usage rows of job {job} add up to {}, and its allocations freed to {}",
                written.text(),
                own.text()
            );
            found.flag(Invariant::UsageTotalsMatchTerminalJobs, detail);
        }
    }
}
