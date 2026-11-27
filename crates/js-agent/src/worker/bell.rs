//! The session's bell: the worker rings it when what it publishes holds news for the server,
//! so the next heartbeat leaves at once instead of after the interval.
//!
//! News is what the server has not heard and acts on: a job newly listed, whatever its state,
//! that runs now, ended, or holds a new result; a job id newly retired; a taint to report or its clear. A start the worker has only just taken,
//! listed as `starting`, is news: for a host whose starts ride on the heartbeat, the heartbeat
//! that leaves for it is the one the server holds for the host's next placement, so a placement
//! is delivered by it and not by the heartbeat after the interval. The server holds a beat that
//! lists a start for a short bound only, so the start's `running` is not kept behind that hold;
//! for a host that polls dispatch it answers that beat at once, unless such beats come closely
//! one after another. A start already listed as `starting` and unchanged is not news, and a
//! job that is gone from the listing is not either: it went because the server
//! acknowledged it.
//!
//! The bell is the sleeper's ([`Sleeper::bell`](crate::Sleeper::bell)); a worker given none
//! rings nothing. Nothing is lost when the session is not waiting: the bell stays rung until
//! the session's next wait that it may end.

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
    // An entry that is new or changed is news, whatever its state.
    let jobs = now.jobs.iter().filter(|job| !before.jobs.contains(job));
    let retired = now.retired_jobs.iter();
    let retired = retired.filter(|id| !before.retired_jobs.contains(id));
    let taint = now.taint.is_some() && now.taint != before.taint;
    let cleared = now.taint_cleared.is_some() && now.taint_cleared != before.taint_cleared;
    [jobs.count() > 0, retired.count() > 0, taint, cleared]
        .into_iter()
        .any(|news| news)
}

#[cfg(test)]
#[path = "bell_tests.rs"]
mod tests;
