//! A taint that clears itself (`docs/design/taint-self-clearing.md`; the rules are in
//! [`taint`](crate::lifecycle::taint)): the re-checks of an idle host, and the clear when the
//! server has acknowledged the report of it.
//!
//! [`Lifecycle::step`] asks for a re-check on every tick; one runs only when the taint is
//! clearable, the host is idle and `--taint-recheck-ms` has passed since the last. The
//! second clean one in a row makes the taint report its clear (`taint_cleared`, see
//! [`Lifecycle::pending_taint_clear`]); the reply that acknowledges it ends the taint.

use std::time::{SystemTime, UNIX_EPOCH};

use super::Lifecycle;
use super::job::{Job, Stage};
use crate::lifecycle::taint::CLEARED_PREFIX;
use crate::lifecycle::{Event, Finding, Finished, code};

/// The run id the re-checks are given.
const RECHECK_RUN: &str = "taint-recheck";

impl Lifecycle {
    /// The same lifecycle, with `every` as the wait between the re-checks of a taint that may
    /// clear itself: `--taint-recheck-ms`. The default is
    /// [`DEFAULT_TAINT_RECHECK`](crate::lifecycle::taint::DEFAULT_TAINT_RECHECK).
    #[must_use]
    pub fn with_taint_recheck(self, every: std::time::Duration) -> Self {
        Self {
            taint_recheck: every,
            ..self
        }
    }

    /// The taint the next heartbeat must carry in `taint_cleared`: set from the second clean
    /// re-check until the server acknowledges the clear with `taint_cleared_ack`. The host is still
    /// tainted until then.
    pub fn pending_taint_clear(&self) -> Option<js_agent_wire::Taint> {
        self.taint.pending_clear()
    }

    /// The run ids and job ids of the runs and jobs whose checks found the taint (`Blocked`),
    /// in the order of [`Lifecycle::held`]. Their container and scratch directory are gone and
    /// they leave only when the taint clears, so a drain that waits for them waits for nothing
    /// it can see end; a restarted agent reads the taint again from the marker.
    pub fn blocked(&self) -> Vec<String> {
        let blocked = self.jobs.iter().filter(|job| job.stage == Stage::Blocked);
        blocked.map(|job| job.run_id.clone()).collect()
    }

    /// Whether no run is held but those whose checks found the taint (`Blocked`) or that are
    /// over (`Freed`), whose container and scratch directory are gone. A container the agent
    /// could not remove is not a run, but it has tainted the host as `cleanup-failed` when it
    /// was found, which no re-check clears ([`TaintState::rechecks`](crate::lifecycle::taint)).
    fn host_idle(&self) -> bool {
        let over = |job: &Job| matches!(job.stage, Stage::Blocked | Stage::Freed);
        self.jobs.iter().all(over)
    }

    /// Re-checks a taint that may clear itself, when the host is idle and a re-check is due.
    /// The check is the post-job one with nothing live, against the baseline taken before
    /// the job that tainted the host; for a taint read from the marker at start it is
    /// [`CheckHook::run_restored`](crate::lifecycle::CheckHook::run_restored), which also
    /// judges the processes the restart baseline holds. What the check notes is pushed to
    /// `events` as a finding of no run, and does not fail the re-check.
    pub(super) fn recheck_taint(&mut self, events: &mut Vec<Event>) {
        if !self.taint.rechecks() {
            return;
        }
        if !self.host_idle() {
            self.taint.restart_rechecks();
            return;
        }
        let now = self.clock.now_ms();
        if !self.taint.recheck_due(now, self.taint_recheck) {
            return;
        }
        let finished = Finished {
            run_id: RECHECK_RUN,
            live: &[],
            slot: None,
            live_slots: &[],
            held_mounts: &[],
        };
        let verdict = match self.taint.restored() {
            true => self.checks.run_restored(&finished),
            false => self.checks.run(&finished),
        };
        // What the check noted is logged, as after a job; a note does not fail the re-check.
        events.extend(self.checks.notes().into_iter().map(|note| note.event()));
        let now = self.clock.now_ms();
        self.taint
            .rechecked(verdict.is_ok(), now, self.taint_recheck);
    }

    /// The line that says what changed, of the first taint of the host, when the check that
    /// found it gave one: the detail of the finding, with the words "the check could not run"
    /// before it for a check that could not. None while the host is clean, for a taint no
    /// check found (a host job, a container that cannot be removed; the `cgroup-not-found`
    /// taint carries its message) and for one read from the
    /// marker at start, which holds no detail.
    pub fn taint_detail(&self) -> Option<String> {
        self.taint.detail().map(str::to_owned)
    }

    /// Ends the taint when `acked` (a reply's `taint_cleared_ack`) answers the report of its clear:
    /// the records of the owed checks of the `Blocked` runs go, the marker is kept as
    /// `taint.json.cleared-<unix seconds>`, those runs and their slots are freed, and the
    /// clear is a finding. A failure is a finding too and leaves everything as it was: the
    /// clear is reported again, and the next acknowledgement tries again.
    pub(super) fn finish_taint_clear(&mut self, acked: bool, events: &mut Vec<Event>) {
        let Some(held) = self.taint.pending_clear().filter(|_| acked) else {
            return;
        };
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let cleared = self
            .release_blocked()
            .and_then(|()| self.taint.clear(stamp));
        let (code, message) = match cleared {
            Ok(()) => (
                code::TAINT_CLEARED,
                format!(
                    "the host is clean again: the re-checks passed twice, {} ms apart, with nothing \
                     running (reason {}, check {}, run {}); the marker is kept as {CLEARED_PREFIX}{stamp}",
                    self.taint_recheck.as_millis(),
                    held.reason,
                    held.check.as_deref().unwrap_or("-"),
                    held.run_id.as_deref().unwrap_or("-"),
                ),
            ),
            Err(why) => (code::TAINT_CLEAR_FAILED, why.to_string()),
        };
        events.push(Event::Finding(Finding {
            run_id: None,
            code,
            message,
        }));
    }

    /// Frees the runs whose checks found the taint: the record that their checks were owed
    /// goes (a restart would taint the host again for it), the run is freed and its slot
    /// comes back.
    ///
    /// # Errors
    ///
    /// A record could not be removed; no run is freed.
    fn release_blocked(&mut self) -> std::io::Result<()> {
        let blocked: Vec<&str> = self
            .jobs
            .iter()
            .filter(|job| job.stage == Stage::Blocked)
            .map(|job| job.run_id.as_str())
            .collect();
        blocked
            .into_iter()
            .try_for_each(|run_id| self.config.state.settle(run_id))?;
        for job in &mut self.jobs {
            if job.stage == Stage::Blocked {
                job.stage = Stage::Freed;
                job.slot
                    .iter()
                    .for_each(|slot| self.slots.unquarantine(*slot));
            }
        }
        Ok(())
    }
}
