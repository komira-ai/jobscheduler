//! One check: the host is read, compared with the baseline, and the baseline takes what it
//! should. Only a process of the job users' range that the baseline did not have is a
//! finding; every other difference, and every input but the process table that cannot be
//! read, is a [`CheckNote`] (see [`super`] for why).

use std::collections::{BTreeMap, BTreeSet};

use super::modules::changed_modules;
use super::note::named_changes;
use super::own::{
    hosts_own_mount, hosts_own_process, is_live, job_can_be_in, runners_own, under_held,
};
use super::{Integrity, Problem, Process, Snapshot, WATCH_ENTRY_CAP, Watched, code};
use crate::lifecycle::{CheckFinding, CheckNote, Finished};

/// The host as it was read, and the inputs that could not be.
pub(super) struct Reading {
    /// The host. An input that could not be read holds the baseline's value (the process
    /// table, which a check cannot do without, is empty).
    pub(super) snapshot: Snapshot,
    /// The inputs that could not be read, in the order they are read.
    pub(super) unread: Vec<Problem>,
}

/// The process table could not be read, so the check cannot run.
pub(super) struct Blind {
    /// The process table's problem.
    problem: Problem,
    /// The notes of the other inputs that could not be read.
    notes: Vec<CheckNote>,
}

impl Reading {
    /// The host and the notes of the inputs that could not be read, or [`Blind`] when the
    /// process table is one of them.
    pub(super) fn seen(self) -> Result<(Snapshot, Vec<CheckNote>), Blind> {
        let (blind, others): (Vec<Problem>, Vec<Problem>) = self
            .unread
            .into_iter()
            .partition(|problem| problem.code == code::PROCESSES_UNREADABLE);
        let notes = others
            .iter()
            .map(|problem| CheckNote::new(problem.code, &problem.detail))
            .collect();
        match blind.into_iter().next() {
            Some(problem) => Err(Blind { problem, notes }),
            None => Ok((self.snapshot, notes)),
        }
    }
}

/// What a comparison of the host with the baseline found.
struct Compared {
    /// The processes that are a job's leftover, as the finding names them.
    residue: Vec<String>,
    /// The changes that are the host's own, in the order of the table of the module doc.
    notes: Vec<CheckNote>,
}

/// One look at the host after a job.
struct Looked {
    /// The host as it was compared.
    now: Snapshot,
    /// The processes that are a job's leftover.
    residue: Vec<String>,
    /// The notes of the unreadable inputs, then of the changes.
    notes: Vec<CheckNote>,
}

/// `read`, or what `baseline` gives and the reason in `unread` when the input could not be
/// read.
fn keep<T>(
    unread: &mut Vec<Problem>,
    code: &'static str,
    read: Result<T, String>,
    baseline: impl FnOnce() -> T,
) -> T {
    read.unwrap_or_else(|detail| {
        unread.push(Problem { code, detail });
        baseline()
    })
}

/// One note of `code` for the things `seen` names, joined, or none for no things.
fn group(code: &'static str, seen: impl IntoIterator<Item = String>) -> Option<CheckNote> {
    let seen: Vec<String> = seen.into_iter().collect();
    (!seen.is_empty()).then(|| CheckNote::new(code, &seen.join(", ")))
}

impl Integrity {
    /// The host now, with the inputs that could not be read: every input is tried, so the
    /// reasons are all given.
    pub(super) fn observe(&self) -> Reading {
        let base = &self.baseline;
        let mut unread = Vec::new();
        let processes = keep(
            &mut unread,
            code::PROCESSES_UNREADABLE,
            self.view.processes(),
            Vec::new,
        );
        let mounts = keep(
            &mut unread,
            code::MOUNTS_UNREADABLE,
            self.view
                .mounts()
                .map(|mounts| mounts.into_iter().collect::<BTreeSet<_>>()),
            || base.mounts.clone(),
        );
        let users = keep(
            &mut unread,
            code::USERS_UNREADABLE,
            self.view.users(),
            || base.users.clone(),
        );
        let groups = keep(
            &mut unread,
            code::GROUPS_UNREADABLE,
            self.view.groups(),
            || base.groups.clone(),
        );
        let modules = keep(
            &mut unread,
            code::MODULES_UNREADABLE,
            self.view.modules(),
            || base.modules.clone(),
        );
        let mut watched = BTreeMap::new();
        for dir in &self.watch {
            let digest = match self.view.watched(dir) {
                Ok(Watched::Digest(digest)) => Some(digest),
                Ok(Watched::TooMany) => {
                    unread.push(Problem {
                        code: code::WATCH_OVER_BOUND,
                        detail: format!("{}: more than {WATCH_ENTRY_CAP} entries", dir.display()),
                    });
                    None
                }
                Err(detail) => {
                    unread.push(Problem {
                        code: code::WATCH_UNREADABLE,
                        detail,
                    });
                    None
                }
            };
            let kept = digest.or_else(|| base.watched.get(dir).cloned());
            watched.extend(kept.map(|digest| (dir.clone(), digest)));
        }
        Reading {
            snapshot: Snapshot {
                processes,
                mounts,
                users,
                groups,
                modules,
                watched,
            },
            unread,
        }
    }

    /// The host after `job`, what is a job's leftover in it, and the notes. A process that
    /// would be a leftover of the job is looked at again after the settle wait, and only what
    /// of the first look is still there is judged; the second look is the host that is
    /// compared, so what else changed during the wait is part of it. A process that starts
    /// during the wait is judged by the next check, and is not in the baseline this one may
    /// take. `restored` is the re-check of a taint read from the marker at start (see
    /// `Integrity::compare`).
    fn look(&self, job: &Finished<'_>, restored: bool) -> Result<Looked, Blind> {
        let (first, mut notes) = self.observe().seen()?;
        let found = self.compare(&first, job, restored);
        if self.settle.is_zero() || found.residue.is_empty() {
            notes.extend(found.notes);
            return Ok(Looked {
                now: first,
                residue: found.residue,
                notes,
            });
        }
        self.view.settle(self.settle);
        let (mut later, mut notes) = self.observe().seen()?;
        let seen: BTreeSet<(u32, u64)> = first.processes.iter().map(Process::key).collect();
        later
            .processes
            .retain(|process| seen.contains(&process.key()));
        let found = self.compare(&later, job, restored);
        notes.extend(found.notes);
        Ok(Looked {
            now: later,
            residue: found.residue,
            notes,
        })
    }

    /// What differs between the baseline and `now`: the processes of the job users' range
    /// that are a job's leftover, and the notes in the order of the table of the module doc.
    ///
    /// With `restored` (the re-check of a taint read from the marker at start) the baseline
    /// exempts no process: the baseline of this process was taken with whatever the tainting
    /// job left, so every process is judged by the two rules that need no baseline, a uid of
    /// the job users' range and a parent `init` in a group a job can be in. The agent's own
    /// process is the exception.
    fn compare(&self, now: &Snapshot, job: &Finished<'_>, restored: bool) -> Compared {
        let mine = |text: &str| is_live(job, text);
        let known: BTreeSet<(u32, u64)> = match restored {
            true => BTreeSet::new(),
            false => self.baseline.processes.iter().map(Process::key).collect(),
        };
        let strays: Vec<&Process> = now
            .processes
            .iter()
            .filter(|process| {
                !known.contains(&process.key())
                    && Some(process.pid) != self.agent_pid
                    && !mine(&process.cgroup)
                    && !hosts_own_process(&process.cgroup, self.own.as_deref())
                    && !runners_own(process, self.own.as_deref())
            })
            .collect();
        let own = job.slot.and_then(|slot| self.slot_host_uid(slot));
        let live_uids: BTreeSet<u32> = job
            .live_slots
            .iter()
            .filter_map(|slot| self.slot_host_uid(*slot))
            .collect();
        let as_job = strays
            .iter()
            .filter(|stray| self.users.contains(stray.uid) && !live_uids.contains(&stray.uid));
        let daemonised = strays
            .iter()
            .filter(|stray| stray.ppid == 1 && job_can_be_in(&stray.cgroup, self.own.as_deref()));
        let new_mounts = now
            .mounts
            .difference(&self.baseline.mounts)
            .filter(|mount| {
                !(mine(&mount.root)
                    || mine(&mount.point)
                    || mine(&mount.source)
                    || under_held(job, &mount.point)
                    || hosts_own_mount(mount))
            });
        let moved = now
            .watched
            .iter()
            .filter(|(dir, digest)| self.baseline.watched.get(*dir) != Some(*digest));
        let residue = as_job
            .map(|stray| {
                let slot = job
                    .slot
                    .filter(|_| own == Some(stray.uid))
                    .map_or(String::new(), |slot| format!(" (leftover of slot {slot})"));
                format!(
                    "{} is running after job {}{slot}",
                    self.describe(stray, &now.users),
                    job.run_id
                )
            })
            .collect();
        let notes = [
            group(
                code::DAEMONISED_CHILD,
                daemonised.map(|stray| {
                    format!(
                        "{} is a new child of init after job {}",
                        self.describe(stray, &now.users),
                        job.run_id
                    )
                }),
            ),
            group(
                code::NEW_MOUNT,
                new_mounts.map(|mount| {
                    format!("{} ({}) from {}", mount.point, mount.fstype, mount.source)
                }),
            ),
            group(
                code::USER_CHANGED,
                named_changes(&self.baseline.users, &now.users),
            ),
            group(
                code::GROUP_CHANGED,
                named_changes(&self.baseline.groups, &now.groups),
            ),
            group(
                code::MODULE_CHANGED,
                changed_modules(&self.baseline.modules, &now.modules),
            ),
            group(
                code::WATCH_DIR_CHANGED,
                moved.map(|(dir, _)| format!("{} differs from the baseline", dir.display())),
            ),
        ]
        .into_iter()
        .flatten()
        .collect();
        Compared { residue, notes }
    }

    /// The baseline after a check that found a leftover: it takes everything that only the
    /// notes look at (users, groups, modules, mounts, watched directories, and the processes
    /// outside the job users' range), without the neighbours' containers (see
    /// [`Snapshot::without_live`]), so a noted change is logged once and not after every later
    /// check; and it keeps its own processes of the job users' range, so the leftover is
    /// found again.
    fn absorb(&mut self, now: Snapshot, job: &Finished<'_>) {
        let mut next = now.without_live(job);
        next.processes
            .retain(|process| !self.users.contains(process.uid));
        next.processes.extend(
            self.baseline
                .processes
                .iter()
                .filter(|process| self.users.contains(process.uid))
                .cloned(),
        );
        self.baseline = next;
    }

    /// The check after `job`, as [`CheckHook::run`](crate::lifecycle::CheckHook::run), or as
    /// [`CheckHook::run_restored`](crate::lifecycle::CheckHook::run_restored) when `restored`.
    /// The notes of the check are kept for [`CheckHook::notes`](crate::lifecycle::CheckHook::notes).
    pub(super) fn judge(&mut self, job: &Finished<'_>, restored: bool) -> Result<(), CheckFinding> {
        match self.look(job, restored) {
            Err(blind) => {
                self.notes = blind.notes;
                Err(CheckFinding::unchecked(
                    blind.problem.code,
                    &blind.problem.detail,
                ))
            }
            Ok(looked) => {
                self.notes = looked.notes;
                match looked.residue.is_empty() {
                    true => {
                        self.baseline = looked.now.without_live(job);
                        Ok(())
                    }
                    false => {
                        let detail = looked.residue.join(", ");
                        self.absorb(looked.now, job);
                        Err(CheckFinding::residue(code::PROCESS_AS_JOB_USER, &detail))
                    }
                }
            }
        }
    }
}
