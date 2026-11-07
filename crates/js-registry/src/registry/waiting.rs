//! The registry's whole-node wait list as the calls that lock read it: the housekeeping
//! every lock runs, the pass of the queue, and what an operator asks of the list
//! ([`Registry::queue_position`], [`Registry::cancel_request`], [`Registry::reservations`],
//! [`Registry::whole_counts`]).
//!
//! Under a placer that keeps the wait list and the request records
//! ([`crate::Placer::keeps_wait_list`], the engine of `--placement js-sched`) each of them is
//! the placer's answer, and the registry's own reservation machine (`whole/queue.rs`,
//! `whole/requests.rs`, `whole/reserve.rs`, [`Registry::place_waiting`]) is not called; under
//! any other placer, or none, it is the registry's, as it always was.

use std::sync::{MutexGuard, PoisonError};
use std::time::Duration;

use js_lease::Timestamp;
use js_types::{HostId, RequestId};

use super::{Registry, State, WholeCounts};
use crate::Placer;
use crate::whole::ending::Ending;
use crate::whole::reasons::Cause;
use crate::whole::requests::{Rules, whole_settings};
use crate::whole::reserve::{HostFacts, Reservation};
use crate::whole_config::WholeNodeConfig;

impl Registry {
    /// The placer when it keeps the whole-node wait list and the request records
    /// ([`Placer::keeps_wait_list`]).
    pub(super) fn keeper(&self) -> Option<&dyn Placer> {
        let placer = self.shared.placer.get().map(|placer| &**placer);
        placer.filter(|placer| placer.keeps_wait_list())
    }

    /// The registry's state as of `now`: hosts lost for longer than the retention, with
    /// no booking, are forgotten first, so no call sees or counts one; idle request records
    /// go, and so do waiting requests not presented within `regain_grace`. Every call but
    /// [`Registry::check_heartbeat`] (which answers as if they were forgotten) locks here.
    pub(super) fn lock_at(&self, now: Timestamp) -> MutexGuard<'_, State> {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let config = &self.shared.config;
        let placer = self.shared.placer.get();
        let held = |host: &HostId| placer.is_some_and(|placer| placer.holds_work_on(host));
        let timing = (now, config.grace_ms(), config.lost_retention_ms());
        let forgotten = state.forget_lost(timing, &held);
        forgotten.iter().for_each(|host| self.tell_gone(host));
        let rules = Rules::of(config);
        let keeper = self.keeper();
        let horizon = rules.idle_horizon_ms();
        if keeper.is_none() {
            state.requests.sweep(now, horizon);
        }
        let idle = Duration::from_millis(horizon);
        placer
            .into_iter()
            .for_each(|placer| placer.whole_sweep(idle));
        let grace = rules.regain_grace_ms;
        let unclaimed = match keeper {
            Some(keeper) => keeper.whole_expire(Duration::from_millis(grace)),
            None => {
                let State { marks, waiting, .. } = &mut *state;
                let unclaimed = waiting.expire(now, grace);
                marks.retain_waiting(waiting);
                unclaimed
            }
        };
        state.dropped += unclaimed;
        state
    }

    /// Fails the prepares past their deadline, then runs a pass of the whole-node queue.
    pub(super) fn advance_at(&self, state: &mut State, now: Timestamp) {
        let keeps = self.keeper().is_some();
        let State {
            book,
            records,
            runs,
            requests,
            failed,
            ..
        } = state;
        let rules = Rules::of(&self.shared.config);
        let mut ending = Ending {
            book,
            runs,
            requests: (!keeps).then_some(requests),
            failed,
            rules,
            now,
            placer: self.shared.placer.get().map(|placer| &**placer),
        };
        for (host, record) in records.iter_mut() {
            let timeout = rules.prepare_timeout_ms;
            let overdue = record.exclusive.overdue_with(now, timeout, ending.placer);
            if let Some(lease) = overdue {
                ending.fail(
                    (&mut record.taint, &mut record.exclusive),
                    host,
                    lease,
                    (Cause::Timeout, ""),
                );
            }
        }
        // With no placer, nothing waits and nothing is set aside: nothing to place or drop
        // (a placer is still asked, so it lets go of the requests that left the wait list).
        let placer = self.shared.placer.get();
        if placer.is_none() && state.waiting.is_empty() && state.marks.is_empty() {
            return;
        }
        let settings = whole_settings(&self.shared.config);
        let hosts = self.host_facts(state, now);
        self.run_pass(state, &settings, &hosts, now);
    }

    /// One pass of the whole-node queue: the placer's when it keeps the wait list (handed
    /// the hosts, the lives and the settings, [`Placer::whole_round`]), else the registry's
    /// own ([`Registry::place_waiting`]).
    pub(super) fn run_pass(
        &self,
        state: &mut State,
        settings: &WholeNodeConfig,
        hosts: &[HostFacts],
        now: Timestamp,
    ) {
        match self.keeper() {
            Some(keeper) => {
                let round = self.hosts_round(state, settings, hosts, now);
                // The placer holds the marks it makes: they are read from it.
                let _ = keeper.whole_round(&round);
            }
            None => self.place_waiting(state, settings, hosts, now),
        }
    }

    /// Where whole-node request `id` waits for a host, from the front (zero is next), or
    /// `None` if it is not waiting. A request waits from the first start that finds a
    /// capable host but none free, in the order such starts were first seen; a repeat of
    /// the start is a poll and keeps its place. It stops waiting when it is placed, is
    /// cancelled, is refused, or is not presented for the pool's `regain_grace`.
    ///
    /// The order is advisory: a start places on any eligible host without asking whether
    /// others wait, so a later waiter, or a request not yet on the list, can take a host
    /// that freed before the request at the front presents itself again. The list holds
    /// at most [`crate::MAX_WAITING_REQUESTS`]; a new request past that is answered
    /// `NoCapacity` and not entered. Under a placer that keeps the wait list the place is
    /// the placer's ([`Placer::whole_places`]).
    pub fn queue_position(&self, id: &RequestId) -> Option<usize> {
        let state = self.lock();
        match self.keeper() {
            Some(keeper) => keeper.whole_places().iter().position(|(at, _)| at == id),
            None => state.waiting.position(id),
        }
    }

    /// Takes whole-node request `id` off the wait list at once; whether it was waiting.
    pub fn cancel_request(&self, id: &RequestId) -> bool {
        let mut state = self.lock();
        match self.keeper() {
            Some(keeper) => keeper.whole_cancel(id),
            None => state.waiting.cancel(id),
        }
    }

    /// The hosts set aside for waiting whole-node requests, in host order. A host is set
    /// aside for one request and a request holds one host. A request's next start commits
    /// its lease on the host it holds; until then the host takes no other start. Under a
    /// placer that keeps the queue each mark is read from it ([`Placer::whole_reservations`]).
    pub fn reservations(&self) -> Vec<Reservation> {
        self.marks_now(&self.lock())
    }

    /// Whether `host` is set aside for a waiting request.
    pub fn is_reserved(&self, host: &HostId) -> bool {
        let marks = self.marks_now(&self.lock());
        marks.iter().any(|mark| mark.host == *host)
    }

    /// The hosts reserved and held, and the requests waiting, now.
    pub fn whole_counts(&self) -> WholeCounts {
        let state = self.lock();
        let held = state
            .records
            .values()
            .filter(|r| r.exclusive.held_since().is_some());
        let queued = match self.keeper() {
            Some(keeper) => keeper.whole_places().len(),
            None => state.waiting.len(),
        };
        WholeCounts {
            reserved: self.marks_now(&state).len(),
            held: held.count(),
            queued,
        }
    }

    /// The calls the registry's reservation machine has had since the registry was made: its
    /// wait list, its request records, its marks and its own passes. Read without the
    /// housekeeping of a lock, which would call the machine itself.
    #[cfg(feature = "test-support")]
    pub fn machine_calls(&self) -> crate::MachineCalls {
        let state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        crate::MachineCalls {
            wait_list: state.waiting.calls.count(),
            records: state.requests.calls.count(),
            marks: state.marks.2.count(),
            passes: state.passes.count(),
        }
    }
}
