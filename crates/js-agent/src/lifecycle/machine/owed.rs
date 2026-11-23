//! The checks an earlier process owed: the `owed-*` records found when the agent starts (see
//! [`taint`](crate::lifecycle::taint)). A process that ended inside a job's cleanup window (an
//! update's hand-over or drain whose bound passed with a cleanup under way, a second stop, a
//! crash) leaves one for each job whose checks had not run. The new process runs those checks
//! itself, once it has taken the containers over, by the rules that need no baseline; it
//! taints the host only when it cannot run them or they find something.

use js_agent_wire::Taint;

use super::Lifecycle;
use crate::lifecycle::runtime::RunHandle;
use crate::lifecycle::taint::reason;
use crate::lifecycle::{CheckFinding, Event, Finding, Finished, code};

/// The taint of the records of owed checks: the run it names (none when the records could not
/// be listed), and the check that says why the records were not settled, when one does.
fn owed_taint(run_id: Option<String>, check: Option<&str>) -> Taint {
    Taint {
        reason: reason::CHECK_OWED.to_owned(),
        run_id,
        check: check.map(str::to_owned),
    }
}

impl Lifecycle {
    /// Reads the records of owed checks when the agent starts, before any container is taken
    /// over, and returns the runs whose checks [`Lifecycle::check_owed`] is to run. The host is
    /// tainted (`check-owed`) at once, and nothing is returned, when the records cannot be
    /// listed (`owed-records-unreadable`), when one is not complete (`owed-record-incomplete`,
    /// naming that run), or when the hook cannot judge without a baseline (naming the first
    /// run, no check): what the jobs left behind may already be part of the baseline the new
    /// process took.
    pub(super) fn owed_at_start(&mut self, events: &mut Vec<Event>) -> Vec<String> {
        let state = &self.config.state;
        let (runs, taint) = match state.owed() {
            Err(_) => (Vec::new(), owed_taint(None, Some(code::OWED_UNREADABLE))),
            Ok(runs) => {
                let first = runs.first().cloned();
                let incomplete = runs.iter().find(|run| !state.owed_complete(run)).cloned();
                match (first, incomplete, self.checks.checks_owed_at_start()) {
                    (None, _, _) => return runs,
                    (Some(_), Some(run), _) => (
                        Vec::new(),
                        owed_taint(Some(run), Some(code::OWED_INCOMPLETE)),
                    ),
                    (Some(first), None, false) => (Vec::new(), owed_taint(Some(first), None)),
                    (Some(_), None, true) => return runs,
                }
            }
        };
        self.seal(taint, None, events);
        runs
    }

    /// Runs the checks the runs `owed` of an earlier process owed, once the containers are
    /// taken over and the leftovers swept: one check of the host
    /// ([`CheckHook::run_restored`](crate::lifecycle::CheckHook::run_restored), the rules that
    /// need no baseline), with every container taken over or that could not be removed live.
    /// What it notes is logged as findings of no run. A check that passes removes every record
    /// and logs `owed-checked`. The host is tainted (`check-owed`, naming the first run), with
    /// the records kept, when the check finds something (the check is the finding's code, the
    /// detail what it found) or could not run, or when a record cannot be removed
    /// (`owed-record-failed`).
    pub(super) fn check_owed(&mut self, owed: &[String], events: &mut Vec<Event>) {
        let Some(first) = owed.first() else {
            return;
        };
        // Every job taken over is running: its container is on the host.
        let live: Vec<RunHandle> = self
            .jobs
            .iter()
            .filter_map(|job| job.stage.container().cloned())
            .chain(self.stuck.iter().map(|held| held.id.clone()))
            .collect();
        let live_slots: Vec<u32> = self
            .jobs
            .iter()
            .filter_map(|job| job.slot)
            .chain(self.stuck.iter().filter_map(|held| held.slot))
            .collect();
        let held_mounts: Vec<std::path::PathBuf> = self
            .jobs
            .iter()
            .filter_map(|job| self.runtime.mounts_of(&job.run_id))
            .collect();
        let verdict = self.checks.run_restored(&Finished {
            run_id: first,
            live: &live,
            slot: None,
            live_slots: &live_slots,
            held_mounts: &held_mounts,
        });
        events.extend(self.checks.notes().into_iter().map(|note| note.event()));
        let state = &self.config.state;
        let (check, detail) = match verdict {
            Ok(()) => match owed.iter().try_for_each(|run| state.settle(run)) {
                Ok(()) => {
                    events.push(Event::Finding(Finding {
                        run_id: None,
                        code: code::OWED_CHECKED,
                        message: format!(
                            "the post-job checks owed by {} run(s) of an earlier process ({first} \
                             first) ran at the start and passed; their records are removed",
                            owed.len()
                        ),
                    }));
                    return;
                }
                Err(why) => (
                    code::OWED_FAILED.to_owned(),
                    format!("a record of owed checks cannot be removed: {why}"),
                ),
            },
            Err(found @ CheckFinding::Residue { .. }) => {
                (found.code().to_owned(), found.detail().to_owned())
            }
            Err(found @ CheckFinding::Unchecked { .. }) => (
                found.code().to_owned(),
                format!("the check could not run: {}", found.detail()),
            ),
        };
        let taint = owed_taint(Some(first.clone()), Some(&check));
        self.seal(taint, Some(detail), events);
    }
}
