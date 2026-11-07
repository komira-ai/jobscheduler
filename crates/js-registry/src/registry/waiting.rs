//! The whole-node wait list as the calls that lock read it: the housekeeping every lock
//! runs, the pass of the queue, and what an operator asks of the list
//! ([`Registry::queue_position`], [`Registry::cancel_request`], [`Registry::reservations`],
//! [`Registry::whole_counts`]).
//!
//! The placer keeps the wait list, the request records and the hosts set aside (the engine
//! of `js-sched-placer`): each of these calls is the placer's answer, and the registry
//! keeps no queue of its own.

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

impl Registry {
    /// The placer given at construction: it picks the host of a start and keeps the
    /// whole-node queue.
    pub(super) fn placer(&self) -> &dyn Placer {
        &*self.shared.placer
    }

    /// The registry's state as of `now`: hosts lost for longer than the retention, with
    /// no booking, are forgotten first, so no call sees or counts one; the placer drops its
    /// idle request records and the waiting requests not presented within `regain_grace`.
    /// Every call but [`Registry::check_heartbeat`] (which answers as if they were
    /// forgotten) locks here.
    pub(super) fn lock_at(&self, now: Timestamp) -> MutexGuard<'_, State> {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let config = &self.shared.config;
        let placer = self.placer();
        let held = |host: &HostId| placer.holds_work_on(host);
        let timing = (now, config.grace_ms(), config.lost_retention_ms());
        let forgotten = state.forget_lost(timing, &held);
        forgotten.iter().for_each(|host| self.tell_gone(host));
        let rules = Rules::of(config);
        placer.whole_sweep(Duration::from_millis(rules.idle_horizon_ms()));
        let grace = Duration::from_millis(rules.regain_grace_ms);
        state.dropped += placer.whole_expire(grace);
        state
    }

    /// Fails the prepares past their deadline, then runs a pass of the whole-node queue.
    pub(super) fn advance_at(&self, state: &mut State, now: Timestamp) {
        let State {
            book,
            records,
            runs,
            failed,
            ..
        } = state;
        let rules = Rules::of(&self.shared.config);
        let mut ending = Ending {
            book,
            runs,
            failed,
            rules,
            now,
            placer: self.placer(),
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
        let settings = whole_settings(&self.shared.config);
        let hosts = self.host_facts(state, now);
        self.run_pass(state, &settings, &hosts, now);
    }

    /// One pass of the whole-node queue: the placer's, handed the hosts, the lives and the
    /// settings ([`Placer::whole_round`]). The placer holds the marks it makes: they are
    /// read from it ([`Placer::whole_reservations`]).
    pub(super) fn run_pass(
        &self,
        state: &State,
        settings: &crate::WholeNodeConfig,
        hosts: &[HostFacts],
        now: Timestamp,
    ) {
        let round = self.hosts_round(state, settings, hosts, now);
        let _ = self.placer().whole_round(&round);
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
    /// `NoCapacity` and not entered. The place is the placer's ([`Placer::whole_places`]).
    pub fn queue_position(&self, id: &RequestId) -> Option<usize> {
        let _state = self.lock();
        let places = self.placer().whole_places();
        places.iter().position(|(at, _)| at == id)
    }

    /// Takes whole-node request `id` off the wait list at once; whether it was waiting.
    pub fn cancel_request(&self, id: &RequestId) -> bool {
        let _state = self.lock();
        self.placer().whole_cancel(id)
    }

    /// The hosts set aside for waiting whole-node requests, in host order. A host is set
    /// aside for one request and a request holds one host. A request's next start commits
    /// its lease on the host it holds; until then the host takes no other start. Each mark is
    /// the placer's ([`Placer::whole_reservations`]).
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
        WholeCounts {
            reserved: self.marks_now(&state).len(),
            held: held.count(),
            queued: self.placer().whole_places().len(),
        }
    }
}
