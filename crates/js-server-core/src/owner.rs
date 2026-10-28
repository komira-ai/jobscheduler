//! A daemon's session and its node's owner, when several frontends share one state store.
//!
//! A daemon is connected to one frontend at a time, and only that frontend may write the
//! leases of the daemon's node ([`js_store::Rows::arrive`] and the checks of every lease
//! write). A daemon's `Hello` makes this core the node's owner, takes up the leases the
//! store holds for it that the scheduler does not, and writes the daemon's `session/` row,
//! which the core renews while the daemon heartbeats. The core acknowledges a heartbeat only
//! while it owns the node and has written the node's session row within
//! [`ACK_WINDOW`](js_store::ACK_WINDOW): a daemon stops its work `SELF_FENCE` after the last
//! heartbeat that was acknowledged, so a core that cannot tell whether the daemon still
//! belongs to it, or whether the other frontends can see it alive, stops acknowledging.

use std::time::Duration;

use js_sched::DaemonInstance;
use js_sched::fence::HANDOVER_GRACE;
use js_store::{ACK_WINDOW, Arrival, OWNER_CACHE_TTL, SESSION_RENEWAL, WorkerUp};
use js_types::sched::{FarmTime, WorkerId};

use crate::adapter::Adapter;
use crate::engine::Engine;
use crate::host::Reported;
use crate::write::Attempt;

/// How long the leases a daemon that moved to this core does not list are kept after it
/// registers, when the daemon process may be another than the one that ran them: the
/// handover grace, which ends when the old process has fenced, and the time the core it left
/// may go on acknowledging the old process ([`OWNER_CACHE_TTL`]).
const OWNER_HANDOVER_WAIT: Duration = HANDOVER_GRACE.saturating_add(OWNER_CACHE_TTL);

impl Engine {
    /// A daemon said `Hello` here as `worker`, whose newest stream is `instance`'s: this
    /// core becomes its owner, takes up the leases the store holds for it that the
    /// scheduler does not hold (a drain of the node that ended at the start, for want of a
    /// lease, is put back in force: [`Engine::resume_drain`]), and reads what the node's
    /// session row said before the registration replaces it. Call it before the registration is written and the worker
    /// is fed to the scheduler.
    ///
    /// The leases of a node whose owner was another frontend are kept for
    /// [`OWNER_HANDOVER_WAIT`] after the registration when the daemon process may be another
    /// than the one the other frontend heard (the session row names another, or has
    /// expired): that process may go on running them, acknowledged by the other frontend,
    /// until it fences, so a heartbeat that leaves them out is not taken as proof that they
    /// stopped.
    ///
    /// A claim the store did not take because it could not be reached is tried again, while
    /// the node is connected, when a feed finds the store reachable ([`Engine::claim_pending`]).
    pub(crate) fn claim_worker(
        &mut self,
        a: &mut dyn Adapter,
        now: FarmTime,
        worker: &WorkerId,
        instance: &DaemonInstance,
    ) {
        let Some(arrival) = self.take_arrival(now, worker, instance) else {
            return;
        };
        for op in arrival.leases {
            // An operation the scheduler finished and retired, whose finish is not written
            // yet, is not taken in again: it would run twice.
            if !self.known_operation(op.operation) {
                self.take_in_stored(a, op);
            }
        }
        self.resume_drain(now, worker);
        let moved = arrival
            .previous
            .is_some_and(|owner| owner.frontend != arrival.frontend);
        let same_process = arrival
            .session
            .as_ref()
            .is_some_and(|old| old.same_as(instance));
        if moved && !same_process {
            self.sched
                .hold_recovered_leases(now, worker, OWNER_HANDOVER_WAIT);
        }
    }

    /// What the store says of `worker` once this core owns it; `None` if the core has no
    /// store, or the store did not take the claim. A claim refused because the store could
    /// not be reached is kept to be made again, in place of an older one for the node, which
    /// was made for a daemon process that has been replaced.
    fn take_arrival(
        &mut self,
        now: FarmTime,
        worker: &WorkerId,
        instance: &DaemonInstance,
    ) -> Option<Arrival> {
        self.unclaimed.remove(worker);
        let mut arrival = None;
        let attempt = self.attempt("an owner claim", &mut |rows| {
            arrival = Some(rows.arrive(worker, now)?);
            Ok(())
        });
        if attempt == Attempt::Transient {
            self.unclaimed.insert(worker.clone(), instance.clone());
        }
        arrival
    }

    /// Claims the nodes whose `Hello` found the state store unreachable, and whose streams
    /// are still open, if the store is reachable now. Called at the start of every feed.
    pub(crate) fn claim_pending(&mut self, a: &mut dyn Adapter, now: FarmTime) {
        if self.store_down {
            return;
        }
        for (worker, instance) in std::mem::take(&mut self.unclaimed) {
            let open = self
                .links
                .get(&worker)
                .is_some_and(|link| a.stream_open(&worker, link.stream));
            if open {
                self.claim_worker(a, now, &worker, &instance);
            }
        }
    }

    /// Whether this core owns `worker` now: a heartbeat of a node it does not own is neither
    /// acknowledged nor a reason to renew the node's session row. A core with no state
    /// store, or one that handed its work over, owns every node it hears from.
    pub(crate) fn owns_worker(&mut self, now: FarmTime, worker: &WorkerId) -> bool {
        if self.fenced || self.rows.is_none() {
            return true;
        }
        let mut owns = false;
        self.attempt("an owner check", &mut |rows| {
            owns = rows.owns(worker, now)?;
            Ok(())
        });
        owns
    }

    /// Whether the session row of `worker` was written, at its registration or a renewal,
    /// within [`ACK_WINDOW`] of `now`. A core with no state store, or one that handed its
    /// work over, writes none and needs none.
    fn session_fresh(&self, now: FarmTime, worker: &WorkerId) -> bool {
        if self.fenced || self.rows.is_none() {
            return true;
        }
        let Some(written) = self.session_ok.get(worker) else {
            return false;
        };
        now < written.saturating_add(ACK_WINDOW)
    }

    /// Renews `worker`'s session row if its last write is a renewal interval old: one write
    /// per daemon session, never one per lease. Called for a heartbeat taken.
    fn renew_session(&mut self, now: FarmTime, worker: &WorkerId) {
        // A fenced core writes nothing: the successor's rows are the truth.
        if self.rows.is_none() || self.fenced {
            return;
        }
        let due = self
            .renewed
            .get(worker)
            .is_none_or(|at| now >= at.saturating_add(SESSION_RENEWAL));
        if !due {
            return;
        }
        let renewed = self.attempt("a session renewal", &mut |rows| rows.renew(worker));
        if renewed == Attempt::Taken {
            self.renewed.insert(worker.clone(), now);
            self.session_ok.insert(worker.clone(), now);
        }
    }

    /// Whether a heartbeat of `worker` taken at `now` is acknowledged: this core owns the
    /// node (and then renews its session row), and wrote the row lately.
    pub(crate) fn acknowledge(&mut self, now: FarmTime, worker: &WorkerId) -> bool {
        let owns = self.owns_worker(now, worker);
        if owns {
            self.renew_session(now, worker);
        }
        owns && self.session_fresh(now, worker)
    }

    /// Writes the session row of a daemon that registered.
    pub(crate) fn record_session(
        &mut self,
        now: FarmTime,
        worker: &WorkerId,
        instance: &DaemonInstance,
        predecessor_stopped: bool,
        reported: &Reported,
    ) {
        if self.rows.is_none() {
            return;
        }
        let up = WorkerUp {
            worker: worker.clone(),
            instance: instance.clone(),
            predecessor_stopped,
            capacity: reported.capacity,
            caps: reported.caps.clone(),
        };
        let written = self.attempt("a session", &mut |rows| rows.session(&up));
        // A registration the store refused is written at the next heartbeat, which renews:
        // a daemon whose row is not there is not acknowledged.
        if written == Attempt::Taken {
            self.renewed.insert(worker.clone(), now);
            self.session_ok.insert(worker.clone(), now);
        }
    }
}
