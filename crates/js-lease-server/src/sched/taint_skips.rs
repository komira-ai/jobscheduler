//! The log line of a live host the pass over the queued jobs skips for the taint the server
//! holds for it ([`TaintSkips`]).
//!
//! A tainted host is offered to no job, and the pass says nothing about a host it does not
//! offer, so an operator whose job stays queued could not tell from the journal that the
//! host is closed or why. The pass hands the live tainted hosts it read to
//! [`TaintSkips::note`], which logs each once for its taint: again only when the host's taint
//! changed, or when the host left the set (it was cleared, or lost) and came back tainted.
//! A pass runs on every tick and every heartbeat, so a line per pass would drown the journal.

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};

use js_agent_wire::Taint;
use js_types::HostId;
use tracing::warn;

/// The taint of each host the pass has said it skips, as it said it.
#[derive(Default)]
pub(crate) struct TaintSkips {
    logged: Mutex<BTreeMap<HostId, Taint>>,
}

impl TaintSkips {
    /// Logs, as a warning with the host, the reason, the check and the run id, each host of
    /// `tainted` (the live hosts a pass read that hold a taint, with the taint) whose taint
    /// is not the one logged for it, and remembers it; forgets every host logged before that
    /// is not in `tainted`, so a taint the host holds after it left the set is logged again.
    pub(crate) fn note<'a>(&self, tainted: impl IntoIterator<Item = (&'a HostId, &'a Taint)>) {
        let tainted: BTreeMap<&HostId, &Taint> = tainted.into_iter().collect();
        let mut logged = self.logged.lock().unwrap_or_else(PoisonError::into_inner);
        logged.retain(|host, _| tainted.contains_key(host));
        for (host, taint) in tainted {
            if logged.get(host) != Some(taint) {
                warn!(
                    %host,
                    reason = taint.reason.as_str(),
                    check = taint.check.as_deref(),
                    run_id = taint.run_id.as_deref(),
                    "a live host is skipped by placement for the taint the server holds"
                );
                logged.insert(host.clone(), taint.clone());
            }
        }
    }
}

#[cfg(test)]
#[path = "taint_skips_tests.rs"]
mod tests;
