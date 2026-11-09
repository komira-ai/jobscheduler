//! The placer side of the registry: the housekeeping every locking call runs, the tick, and
//! what the placer is told of the hosts ([`crate::Placer::host_up`],
//! [`crate::Placer::heard`], [`crate::Placer::host_gone`]).

use std::sync::{MutexGuard, PoisonError};

use js_types::Timestamp;
use js_types::sched::usage::NodeLoad;
use js_types::{HostId, Shape};

use super::{Registry, State};
use crate::Placer;

impl Registry {
    /// The placer given at construction.
    pub(super) fn placer(&self) -> &dyn Placer {
        &*self.shared.placer
    }

    /// The registry's state as of `now`: hosts lost for longer than the retention, that the
    /// placer holds no work on, are forgotten first, so no call sees or counts one. Every call
    /// but [`Registry::check_heartbeat`] (which answers as if they were forgotten) locks here.
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
        state
    }

    /// The registry's housekeeping as of now: forgets the hosts lost past the retention that
    /// the placer holds no work on, as every locking call does first. The server runs it on
    /// each tick, so a silent host leaves even when nothing else calls the registry.
    pub fn advance(&self) {
        drop(self.lock());
    }

    /// Tells the placer that `host` said hello, listed in the book as
    /// `shape`: the pool's booked shape with what of the host's claim the pool places by
    /// (`PoolConfig::list_claimed`).
    pub(super) fn tell_up(&self, host: &HostId, shape: &Shape) {
        self.placer().host_up(host, shape);
    }

    /// Tells the placer that `host` is no longer listed.
    pub(super) fn tell_gone(&self, host: &HostId) {
        self.placer().host_gone(host);
    }

    /// Tells the placer that a heartbeat of `host` was accepted, with
    /// the reading it brought ([`crate::Placer::heard`]).
    pub(super) fn tell_heard(&self, host: &HostId, reading: Option<&NodeLoad>) {
        self.placer().heard(host, reading);
    }
}
