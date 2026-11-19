//! The session's bell: the worker rings it when what it publishes holds news for the server,
//! so the next heartbeat leaves at once instead of after the interval.
//!
//! News is what the server has not heard and acts on: a run or a job that runs now, ended,
//! or holds a new result; a result not yet reported; a job id newly retired; a taint to
//! report or its clear. A start the worker has only just taken, listed as `starting`, is not
//! news: the server sends no start again while it waits for the reply it sent the start in,
//! and a heartbeat sent then would only be held at the server, where the start's `running`
//! could not ride on it until the hold ends. A run or a job that is gone from the listing is
//! not news either: it went because the server acknowledged it.
//!
//! The bell is the sleeper's ([`Sleeper::bell`](crate::Sleeper::bell)); a worker given none
//! rings nothing. Nothing is lost when the session is not waiting: the bell stays rung until
//! the session's next wait that it may end.

use js_agent_wire::HeldState;

use super::{Shared, Snapshot, Worker};
use crate::lifecycle::Wake;

impl Worker {
    /// Rings `bell` whenever what the worker publishes holds news for the server (see the
    /// module `bell`). Given once; a later call changes nothing.
    pub fn ring_on_news(&self, bell: Wake) {
        let _ = self.handle.shared.bell.set(bell);
    }
}

impl Shared {
    /// Publishes `snapshot` as [`Shared::publish`] does, and rings the bell when it holds news
    /// against `told`, the snapshot published before it, which it then replaces.
    pub(super) fn publish_news(
        &self,
        told: &mut Snapshot,
        (snapshot, answers, lines): (Snapshot, &super::LeaseAnswers, Vec<String>),
    ) {
        let news = holds_news(told, &snapshot);
        told.clone_from(&snapshot);
        self.publish(snapshot, answers, lines);
        if news {
            self.bell.get().iter().for_each(|bell| bell.wake());
        }
    }
}

/// Whether `now` holds something the server has not heard against `before` (see the module
/// `bell`).
pub(super) fn holds_news(before: &Snapshot, now: &Snapshot) -> bool {
    // A changed entry is news unless it is a start just taken: `starting`, and not listed
    // before.
    let runs = now.held.iter().filter(|run| !before.held.contains(run));
    let runs = runs.filter(|run| {
        let known = || before.held.iter().any(|was| was.run_id == run.run_id);
        run.state != HeldState::Starting || known()
    });
    let jobs = now.jobs.iter().filter(|job| !before.jobs.contains(job));
    let jobs = jobs.filter(|job| {
        let known = || before.jobs.iter().any(|was| was.job_id == job.job_id);
        job.state != HeldState::Starting || known()
    });
    let results = now
        .results
        .iter()
        .filter(|result| !before.results.contains(result));
    let retired = now.retired_jobs.iter();
    let retired = retired.filter(|id| !before.retired_jobs.contains(id));
    let taint = now.taint.is_some() && now.taint != before.taint;
    let cleared = now.taint_cleared.is_some() && now.taint_cleared != before.taint_cleared;
    [
        runs.count() > 0,
        jobs.count() > 0,
        results.count() > 0,
        retired.count() > 0,
        taint,
        cleared,
    ]
    .into_iter()
    .any(|news| news)
}

#[cfg(test)]
#[path = "bell_tests.rs"]
mod tests;
