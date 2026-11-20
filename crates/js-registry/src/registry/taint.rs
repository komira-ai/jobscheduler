//! The taint a failed prepare of a whole-host job puts on its host, and the clear an
//! operator makes.
//!
//! A whole-host job whose prepare the agent refuses `host-unsafe` taints its host; the
//! placement engine that fails the prepare asks for the taint here. A taint ends by the
//! host's own word ([`Registry::heartbeat`]: its report that it cleared the taint, or its
//! saying it holds none when the taint is one the agent made, which a `host-unsafe` taint is
//! not), or by [`Registry::clear_taint`], which the operator's call makes after the caller has
//! written the binding without it, and which is the only end of a `host-unsafe` taint and of
//! a host job's.

use js_agent_wire::Taint;
use js_types::HostId;

use super::Registry;
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
}
