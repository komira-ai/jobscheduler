//! What a server keeps for the dispatch poll ([`crate::agent_api`]): its timings, the wakes of
//! the polls it holds, and the name it claims messages by.
//!
//! The wakes are their own ([`crate::held_beat::Wakes`] keeps one watch per host, and a later
//! watch takes over from an earlier one): a host that polls dispatch also has its heartbeat
//! held, and each needs a wake of its own when a job is placed on the host, so the pass wakes
//! both ([`DispatchState::wakes`] beside the heartbeat's), as another server's notice of a
//! message for the host does ([`crate::listening`]).

use std::sync::OnceLock;

use crate::config::DispatchTimes;
use crate::held_beat::Wakes;

/// The name a server claims messages by until [`DispatchState::set_claimer`] gives it a
/// better one (a server a test builds without a listener).
const UNNAMED: &str = "js-server";

/// The dispatch poll's timings, wakes and claimer name.
pub(crate) struct DispatchState {
    /// How long a claim holds a message, and how a held poll is timed.
    pub(crate) times: DispatchTimes,
    /// The polls held open for a message, one watch per host; every placement the pass
    /// commits wakes the host's, and so does another server's notice for the host.
    pub(crate) wakes: Wakes,
    /// The name this server claims messages by, set once the server knows where it listens.
    claimer: OnceLock<String>,
}

impl DispatchState {
    /// The state of a server whose polls are timed by `times`.
    pub(crate) fn new(times: DispatchTimes) -> Self {
        Self {
            times,
            wakes: Wakes::default(),
            claimer: OnceLock::new(),
        }
    }

    /// Names this server for the claims it makes ([`js_jobs::DispatchMessage::claimed_by`]):
    /// the address it serves agents on and its process id, which tells the servers of one
    /// store apart in a message's row. Kept for diagnosis alone: nothing decides by it, so
    /// two servers that happen to share a name (two containers of one address and one pid)
    /// still never hand a message out twice. The first name given is kept.
    pub(crate) fn set_claimer(&self, name: String) {
        let _ = self.claimer.set(name);
    }

    /// The name this server claims messages by.
    pub(crate) fn claimer(&self) -> &str {
        self.claimer.get().map_or(UNNAMED, String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a claimer name that a later call replaces, so one server's claims would carry
    /// two names, and a server with no name that writes an empty one. A server claims by the
    /// name it was given, the first one given stays, and one never named claims as `js-server`.
    #[test]
    fn the_first_name_given_is_the_claimers() {
        let state = DispatchState::new(DispatchTimes::default());
        assert_eq!(state.claimer(), UNNAMED);
        state.set_claimer("127.0.0.1:7301/42".to_owned());
        state.set_claimer("other".to_owned());
        assert_eq!(state.claimer(), "127.0.0.1:7301/42");
    }
}
