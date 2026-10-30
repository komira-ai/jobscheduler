#![allow(dead_code, unused_imports, unused_variables)]
//! What the agent says about its bundles, and the disk it keeps back for them.

use js_agent_wire::limits::{BUNDLE_CAPABILITY, MAX_BUNDLE_CAPABILITY_DIGESTS};
use js_agent_wire::{Caps, bundle_capability};
use js_caps::BUNDLE_ROOT;

use super::host::BundleHost;
use super::settings::Settings;

/// The facts about the store, and what went wrong reading them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reported {
    /// The frame for a local consumer.
    pub caps: Caps,
    /// Warnings for the log: a store that could not be listed reports no bundle.
    pub warnings: Vec<String>,
}

/// The facts a host with `settings` reports: the manifest digests the store holds, its root,
/// the read model and the Xcode builds the host found. A store that cannot be listed
/// reports no bundle and warns, so a job is never placed on a guess.
pub fn report(settings: &Settings, host: &dyn BundleHost) -> Reported {
    Reported {
        caps: Caps::default(),
        warnings: Vec::new(),
    }
}

/// The capabilities of the hello for `caps`: `bundle-root` always, and `bundle` (the
/// digests, comma-separated; the first of them when there are more than the wire takes)
/// when the store holds any.
pub fn hello_capabilities(caps: &Caps) -> Vec<(String, String)> {
    Vec::new()
}

/// The bytes of the budget the store has not used yet: `max(0, budget - used)`. The disk
/// offered to jobs leaves them out, because the store will take them. A store whose use
/// cannot be read is taken as empty (the whole budget is kept back) with a warning.
pub fn reserve_bytes(settings: &Settings, host: &dyn BundleHost) -> (u64, Option<String>) {
    (0, None)
}
