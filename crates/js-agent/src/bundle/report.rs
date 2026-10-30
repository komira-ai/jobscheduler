//! What the agent says about its bundles, and the disk it keeps back for them.

use js_agent_wire::limits::{BUNDLE_CAPABILITY, MAX_BUNDLE_CAPABILITY_DIGESTS};
use js_agent_wire::{Caps, bundle_capability};
use js_caps::BUNDLE_ROOT;

use std::collections::BTreeSet;
use std::path::Path;

use super::host::BundleHost;
use super::list::parse_digests;
use super::settings::{Settings, Trust};

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
pub fn report(settings: &Settings, host: &dyn BundleHost, long_values: bool) -> Reported {
    if !host.can_run() {
        let why = "--bundle-store: this agent cannot run a bundle yet, so it reports no bundle \
                   and no bundle-root";
        return Reported {
            caps: Caps::default(),
            warnings: vec![why.to_owned()],
        };
    }
    let mut warnings = Vec::new();
    let held = host.held(&settings.store).unwrap_or_else(|why| {
        warnings.push(format!(
            "--bundle-store: cannot list the bundles ({why}); none is reported"
        ));
        Vec::new()
    });
    let bundles = permitted(settings, host, held).unwrap_or_else(|why| {
        warnings.push(why);
        Vec::new()
    });
    let take = hello_digests(long_values);
    if bundles.len() > take {
        warnings.push(format!(
            "--bundle-store: holds {} bundles; only the first {take} are reported in the hello",
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

/// How many digests the hello carries: one fits the common 128-byte cap on a capability
/// value, and more only go to a server that has said it takes a longer one.
fn hello_digests(long_values: bool) -> usize {
    [1, MAX_BUNDLE_CAPABILITY_DIGESTS][usize::from(long_values)]
}

/// A list file as a set, or the reason it cannot be used.
fn list(host: &dyn BundleHost, file: &Path, what: &str) -> Result<BTreeSet<String>, String> {
    let text = host.read_file(file).map_err(|why| {
        format!("--bundle-store: the {what} cannot be read ({why}); no bundle is reported")
    })?;
    parse_digests(&text).map_err(|why| {
        format!("--bundle-store: the {what} is malformed ({why}); no bundle is reported")
    })
}

/// The held digests the deny list and the allow list let through. A list that cannot be read
/// lets nothing through. A consumer that places work by `bundle` must not be sent to a host
/// that would refuse the digest.
fn permitted(
    settings: &Settings,
    host: &dyn BundleHost,
    held: Vec<String>,
) -> Result<Vec<String>, String> {
    let denied = match &settings.deny_file {
        Some(file) => list(host, file, "deny list")?,
        None => BTreeSet::new(),
    };
    let allowed = match (settings.trust, &settings.allow_file) {
        (Trust::List, Some(file)) => Some(list(host, file, "allow list")?),
        _ => None,
    };
    let listed = |digest: &String| allowed.as_ref().is_none_or(|set| set.contains(digest));
    Ok(held
        .into_iter()
        .filter(|digest| !denied.contains(digest) && listed(digest))
        .collect())
}

/// The capabilities of the hello for `caps`: `bundle-root` when the host has a store to
/// report, and `bundle` (the digests, comma-separated: one, or up to the wire's longest
/// when `long_values`) when it holds any.
pub fn hello_capabilities(caps: &Caps, long_values: bool) -> Vec<(String, String)> {
    let shown = caps.bundles.len().min(hello_digests(long_values));
    let held = (!caps.bundles.is_empty()).then(|| {
        (
            BUNDLE_CAPABILITY.to_owned(),
            bundle_capability(&caps.bundles[..shown]),
        )
    });
    let root =
        (!caps.bundle_root.is_empty()).then(|| (BUNDLE_ROOT.to_owned(), caps.bundle_root.clone()));
    held.into_iter().chain(root).collect()
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
