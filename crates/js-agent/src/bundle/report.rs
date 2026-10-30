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
    let mut warnings = Vec::new();
    let bundles = host.held(&settings.store).unwrap_or_else(|why| {
        warnings.push(format!(
            "--bundle-store: cannot list the bundles ({why}); none is reported"
        ));
        Vec::new()
    });
    if bundles.len() > MAX_BUNDLE_CAPABILITY_DIGESTS {
        warnings.push(format!(
            "--bundle-store: holds {} bundles; only the first {MAX_BUNDLE_CAPABILITY_DIGESTS} are \
             reported in the hello",
            bundles.len()
        ));
    }
    let mut xcode: Vec<String> = host.xcodes().into_iter().map(|found| found.build).collect();
    xcode.sort();
    xcode.dedup();
    let caps = Caps {
        bundles,
        bundle_root: settings.store.to_string_lossy().into_owned(),
        read_model: settings.read_model,
        xcode,
    };
    Reported { caps, warnings }
}

/// The capabilities of the hello for `caps`: `bundle-root` always, and `bundle` (the
/// digests, comma-separated; the first of them when there are more than the wire takes)
/// when the store holds any.
pub fn hello_capabilities(caps: &Caps) -> Vec<(String, String)> {
    let held = (!caps.bundles.is_empty()).then(|| {
        (
            BUNDLE_CAPABILITY.to_owned(),
            bundle_capability(&caps.bundles),
        )
    });
    held.into_iter()
        .chain([(BUNDLE_ROOT.to_owned(), caps.bundle_root.clone())])
        .collect()
}

/// The bytes of the budget the store has not used yet: `max(0, budget - used)`. The disk
/// offered to jobs leaves them out, because the store will take them. A store whose use
/// cannot be read is taken as empty (the whole budget is kept back) with a warning.
pub fn reserve_bytes(settings: &Settings, host: &dyn BundleHost) -> (u64, Option<String>) {
    match host.used_bytes(&settings.store) {
        Ok(used) => (settings.budget_bytes.saturating_sub(used), None),
        Err(why) => (
            settings.budget_bytes,
            Some(format!(
                "--bundle-store: cannot read the bytes the store uses ({why}); its whole budget \
                 is kept back from jobs"
            )),
        ),
    }
}
