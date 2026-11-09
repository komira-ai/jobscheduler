//! The taint a failed prepare of a whole-host job puts on its host.
//!
//! A whole-host job whose prepare the agent refuses `host-unsafe` taints its host; the
//! placement engine that fails the prepare asks for the taint here.

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
}
