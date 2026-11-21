//! The taint a failed prepare of a whole-host job puts on its host, and the clear an
//! operator makes.
//!
//! A whole-host job whose prepare the agent refuses `host-unsafe` taints its host; the
//! placement engine that fails the prepare asks for the taint here. A taint ends by the
//! host's own word ([`Registry::heartbeat`]: its report that it cleared the taint, or its
//! saying it holds none when the taint is one the agent made, which a `host-unsafe` taint is
//! not), or by [`Registry::clear_taint`], which the operator's call makes after the caller has
//! written the binding without it, and which is the only end of a `host-unsafe` taint and of
//! a host job's. A caller that shares the store with other servers takes the taint the store
//! holds, when its commit lost to another server, by [`Registry::take_stored_taint`].

use js_agent_wire::Taint;
use js_types::HostId;

use super::Registry;
use super::beat::agent_made;
use crate::whole::requests::prepare_taint;

impl Registry {
    /// Taints `host` as a refused `host-unsafe` prepare does, when the registry knows the
    /// host and it has no taint yet: the first taint is kept, so a host that already has one
    /// is left as it is. Returns whether the host's taint changed. It is for the failed
    /// prepare of a whole-host job whose agent refused it `host-unsafe`; the host then takes
    /// no new job, and its binding carries the taint ([`Registry::bindings`]), which the caller persists.
    pub fn taint_after_prepare(&self, host: &HostId) -> bool {
        let mut state = self.lock();
        let Some(record) = state.records.get_mut(host) else {
            return false;
        };
        let changed = record.taint.is_none();
        record.taint.get_or_insert_with(prepare_taint);
        changed
    }

    /// Ends the taint of `host`, whoever set it, and returns the taint it held: `None` when
    /// the registry does not hold `host` or the host has none, and then nothing changes. From
    /// then on the host's view ([`Registry::hosts`]) and [`Registry::bindings`] carry no taint
    /// and placement offers the host jobs again. It is for the operator's clear of a taint
    /// the agent's word does not end (a host job's, a refused prepare's, one an agent that
    /// predates `tainted` holds); a caller that persists bindings writes the binding
    /// without the taint first.
    pub fn clear_taint(&self, host: &HostId) -> Option<Taint> {
        let mut guard = self.lock();
        guard.records.get_mut(host)?.taint.take()
    }

    /// Takes `stored`, the taint the host's binding holds in the store a server shares with
    /// others (none when the binding holds none), as the taint of `host`: it replaces the
    /// taint the registry holds, or ends it when `stored` is `None`. It is for a caller whose
    /// conditional commit of the host's taint lost to another server's change, so that the
    /// next heartbeat is judged on what the store holds. A taint the server made itself, a
    /// host job's or a refused prepare's, is never replaced or ended, as the agent's word does
    /// not end it either. Returns whether the host's taint changed: `false` when the registry
    /// does not hold `host`, when the taint held is the server's own, and when it is `stored`
    /// already.
    pub fn take_stored_taint(&self, host: &HostId, stored: Option<Taint>) -> bool {
        let mut guard = self.lock();
        let Some(record) = guard.records.get_mut(host) else {
            return false;
        };
        let server_made = record.taint.as_ref().is_some_and(|held| !agent_made(held));
        if server_made || record.taint == stored {
            return false;
        }
        record.taint = stored;
        true
    }
}
