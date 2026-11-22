//! What bounds a native run through the helper: the deadline the helper holds for the run
//! itself, and the soft stop.
//!
//! - **Deadline.** A start whose labels carry the job's wall limit ([`label::WALL_MS`]) asks
//!   the helper's `run` for a deadline of that limit plus the stop grace: while an agent
//!   runs, its lifecycle stops the job at the wall limit and gives it the grace, and the
//!   helper's kill comes only after both, so it changes nothing then; while no agent runs,
//!   the helper ends the run by itself.
//! - **Soft stop.** [`Runtime::stop`](crate::lifecycle::Runtime::stop) asks the helper's
//!   `stop`: `SIGTERM` to the run's process group, `kill-uid` after the grace.
//!
//! A helper from before the deadline and the `stop` verb refuses either as a request it
//! cannot read and does nothing else ([`ClientError::unknown_to_helper`]). The runtime then
//! remembers that it talks to such a helper and sends what it knows: the `run` without the
//! deadline (the lifecycle's own wall limit still holds while an agent runs), and `kill-uid`
//! for a stop.

use std::os::fd::BorrowedFd;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::Duration;

use js_mac_pool::seatbelt::Profile;
use js_mac_session::client::ClientError;

use super::NativeRuntime;
use super::one_program::Started;
use crate::lifecycle::{RunHandle, RunSpec, RuntimeError, label};

impl NativeRuntime {
    /// The deadline the helper is asked to hold for `spec`: its wall limit and the stop
    /// grace, or none when its labels carry no wall limit.
    fn deadline_of(&self, spec: &RunSpec) -> Option<Duration> {
        let wall = spec.labels.get(label::WALL_MS)?.parse::<u64>().ok()?;
        Some(Duration::from_millis(wall).saturating_add(self.stop_grace))
    }

    /// Starts the run of `lease` (`profile` over `command`, `env`, the four descriptors, in
    /// `cwd`) through the backend, with the deadline of `spec` when it has one and the helper
    /// knows deadlines. Returns the running process and the deadline the helper holds, in
    /// milliseconds.
    pub(super) fn start(
        &self,
        (spec, lease, cwd): (&RunSpec, &str, &Path),
        (profile, command, env): (&Profile, &[String], &[(String, String)]),
        fds: [BorrowedFd<'_>; 4],
    ) -> Result<(Started, Option<u64>), ClientError> {
        let limits = Some(self.limits);
        let deadline = self
            .deadline_of(spec)
            .filter(|_| !self.old_helper.load(Ordering::Relaxed));
        let places = (cwd, spec.scratch.as_path());
        if let Some(deadline) = deadline {
            let run = (profile, command);
            match self
                .backend
                .run(lease, run, env, fds, (limits, Some(deadline)), places)
            {
                Err(why) if why.unknown_to_helper() => {
                    tracing::warn!(%why, "the helper holds no run deadline: the wall limit alone");
                    self.old_helper.store(true, Ordering::Relaxed);
                }
                started => {
                    let held = u64::try_from(deadline.as_millis()).unwrap_or(u64::MAX);
                    return started.map(|running| (running, Some(held)));
                }
            }
        }
        let running =
            self.backend
                .run(lease, (profile, command), env, fds, (limits, None), places)?;
        Ok((running, None))
    }

    /// Stops the run `id`: the backend's `stop` with `grace`, or `kill-uid` at once on a
    /// helper that has no `stop`.
    pub(super) fn soft_stop(
        &mut self,
        id: &RunHandle,
        grace: Duration,
    ) -> Result<(), RuntimeError> {
        if !self.old_helper.load(Ordering::Relaxed) {
            match self.backend.stop(&id.0, grace) {
                Err(why) if why.unknown_to_helper() => {
                    tracing::warn!("the helper has no soft stop ({why}); killing at once");
                    self.old_helper.store(true, Ordering::Relaxed);
                }
                stopped => return self.ended(stopped, "stop"),
            }
        }
        let killed = self.backend.kill_uid(&id.0);
        self.ended(killed, "kill-uid")
    }
}
