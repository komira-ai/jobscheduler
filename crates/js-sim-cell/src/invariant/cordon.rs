//! [`Invariant::CordonNeverKillsRunningJob`]: a cordon takes a host out of placement and
//! does nothing else to the jobs on it.
//!
//! The check reads the operator's cordons ([`Observations::cordons`], as windows of
//! virtual time on one host) beside the allocation rows the cell read, the jobs it read
//! running, and the stops the agents read. It holds three things for each window, from the
//! virtual time the cordon was set (`from`) to the end of the cordon (`to`):
//!
//! - (a) no allocation was created on the host after `from` and before `to`;
//! - (b) no allocation that was running on the host at `from` ended before `to` for a reason
//!   other than its own: the job exited, hit its wall limit or its memory limit, or the
//!   client cancelled it ([`Observations::cancels`], by the item of the job, asked by the
//!   time of the end);
//! - (c) no agent of the host read a stop of such a job before `to`, unless the client had
//!   cancelled it, when the allocation ended for a reason that is not its own.
//!
//! An allocation was running at `from` when the cell read its job running at or before
//! `from` (the cell reads the tables just before it cordons), it was created at or before
//! `from`, and it was not freed before. An allocation the cell never read with its job
//! running is not protected: the cell cannot say it ran. An end is dated by the time the
//! server freed the allocation, or, while it is only releasing, by the time of the read
//! that first showed it so, which is no earlier than the end. A stop the job outlived (an
//! allocation with no end reason) is not an end and is not flagged by (c).
//!
//! The check is a scan of the cordons and the allocation rows; a run with no cordon costs
//! nothing, and cordons are few.

use std::collections::BTreeMap;

use js_jobs::{AllocationId, AllocationStep, EndReason, JobId, JobState};
use js_sim::text::time;
use js_types::sched::FarmTime;

use super::{Found, Invariant};
use crate::cordon::Window;
use crate::jobs::{AllocationRow, JOB_PREFIX};
use crate::observe::{ItemId, Observations};
use crate::text;

/// What the cell read of one allocation: its last row, and when a read first showed it no
/// longer booked.
struct Seen<'a> {
    row: &'a AllocationRow,
    ended_read: Option<FarmTime>,
}

/// `CordonNeverKillsRunningJob` over the cordons and reads of `observations`.
pub(super) fn check(observations: &Observations, found: &mut Found) {
    let windows = observations.windows();
    if windows.is_empty() {
        return;
    }
    let mut seen: BTreeMap<AllocationId, Seen<'_>> = BTreeMap::new();
    for read in &observations.allocation_reads {
        let ended = (read.row.step != AllocationStep::Booked).then_some(read.at);
        let was = seen.get(&read.row.allocation).and_then(|s| s.ended_read);
        let ended_read = was.or(ended);
        let row = &read.row;
        seen.insert(row.allocation, Seen { row, ended_read });
    }
    let mut running: BTreeMap<&JobId, FarmTime> = BTreeMap::new();
    let reads = observations.job_reads.iter();
    for read in reads.filter(|read| read.state == JobState::Running) {
        running.entry(&read.job).or_insert(read.at);
    }
    for window in &windows {
        for one in seen.values() {
            if one.row.host != window.host {
                continue;
            }
            created(window, one.row, found);
            let protected = protected(window, one, &running);
            if protected {
                ended(observations, window, one, found);
                stopped(observations, window, one, found);
            }
        }
    }
}

/// (a): `row` was created on the cordoned host while the cordon held.
fn created(window: &Window, row: &AllocationRow, found: &mut Found) {
    if (row.created_at > window.from) & (row.created_at < window.to) {
        let detail = format!(
            "job {} was given an allocation on host {} at {}, while the host was cordoned from {} until {}",
            row.job,
            text::host(&row.host),
            time(row.created_at),
            time(window.from),
            time(window.to)
        );
        found.flag(Invariant::CordonNeverKillsRunningJob, detail);
    }
}

/// Whether the allocation of `one` was running on the host of `window` when the cordon was
/// set.
fn protected(window: &Window, one: &Seen<'_>, running: &BTreeMap<&JobId, FarmTime>) -> bool {
    let row = one.row;
    let ran = running.get(&row.job).is_some_and(|at| *at <= window.from);
    let there = row.created_at <= window.from;
    let live = row.freed_at.is_none_or(|freed| freed > window.from);
    ran & there & live
}

/// (b): the protected allocation of `one` ended while the cordon held, for a reason that is
/// not its own.
fn ended(observations: &Observations, window: &Window, one: &Seen<'_>, found: &mut Found) {
    let row = one.row;
    let Some(end) = row.freed_at.or(one.ended_read) else {
        return;
    };
    let reason = killed_by(observations, row, end);
    if let Some(reason) = reason.filter(|_| end < window.to) {
        let detail = format!(
            "job {} was running on host {} when it was cordoned at {} and ended at {} as {} while the cordon held until {}",
            row.job,
            text::host(&row.host),
            time(window.from),
            time(end),
            reason_name(reason),
            time(window.to)
        );
        found.flag(Invariant::CordonNeverKillsRunningJob, detail);
    }
}

/// (c): an agent of the host read a stop of the protected job of `one` while the cordon
/// held, and the job ended for a reason that is not its own.
fn stopped(observations: &Observations, window: &Window, one: &Seen<'_>, found: &mut Found) {
    let row = one.row;
    let replies = observations.job_replies.iter();
    let replies = replies.filter(|reply| reply.host == row.host);
    let in_window = replies.filter(|reply| (reply.at >= window.from) & (reply.at < window.to));
    for reply in in_window.filter(|reply| reply.stops.contains(&row.job)) {
        let reason = killed_by(observations, row, reply.at);
        if let Some(reason) = reason {
            let detail = format!(
                "host {} was sent a stop of job {} at {}, which was running there when the host was cordoned at {}, and the job ended as {} with no cancel of the client's before the stop",
                text::host(&row.host),
                row.job,
                time(reply.at),
                time(window.from),
                reason_name(reason)
            );
            found.flag(Invariant::CordonNeverKillsRunningJob, detail);
        }
    }
}

/// The reason `row` ended for, when it is not its own at `at`: the job's own ends are an
/// exit, the wall limit and the memory limit, and a cancel the client had asked for by `at`.
fn killed_by(observations: &Observations, row: &AllocationRow, at: FarmTime) -> Option<EndReason> {
    let asked = asked_cancel(observations, &row.job);
    let own = |reason: &EndReason| match reason {
        EndReason::Exited | EndReason::WallLimit | EndReason::Memory => true,
        EndReason::Cancelled => asked.is_some_and(|asked| asked <= at),
        _ => false,
    };
    row.end_reason.filter(|reason| !own(reason))
}

/// When the client first asked to cancel `job`, by the item the front submitted it for.
fn asked_cancel(observations: &Observations, job: &JobId) -> Option<FarmTime> {
    let number = job.as_str().strip_prefix(JOB_PREFIX);
    let item = number.and_then(|number| number.parse().ok()).map(ItemId);
    item.and_then(|item| observations.cancels.get(&item).copied())
}

/// A reason as a violation spells it.
fn reason_name(reason: EndReason) -> &'static str {
    match reason {
        EndReason::Exited => "exited",
        EndReason::WallLimit => "wall-limit",
        EndReason::Memory => "memory",
        EndReason::Cancelled => "cancelled",
        EndReason::HostAbsent => "host-absent",
        EndReason::FailedPrepare => "failed-prepare",
        EndReason::StartTimeout => "start-timeout",
        EndReason::Refused => "refused",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts each end reason's spelling and that the spellings differ: a violation names
    /// the reason an allocation was ended for. Catches two reasons written alike, which
    /// would hide which of them ended a job on a cordoned host.
    #[test]
    fn every_end_reason_has_its_own_name() {
        let reasons = [
            EndReason::Exited,
            EndReason::WallLimit,
            EndReason::Memory,
            EndReason::Cancelled,
            EndReason::HostAbsent,
            EndReason::FailedPrepare,
            EndReason::StartTimeout,
            EndReason::Refused,
        ];
        let names: Vec<&str> = reasons.into_iter().map(reason_name).collect();
        assert_eq!(
            names,
            [
                "exited",
                "wall-limit",
                "memory",
                "cancelled",
                "host-absent",
                "failed-prepare",
                "start-timeout",
                "refused"
            ]
        );
    }
}
