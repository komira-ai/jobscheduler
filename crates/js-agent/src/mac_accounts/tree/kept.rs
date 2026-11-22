//! The items macOS's own per-user daemons make in a slot's per-user folder that root may not
//! remove, and which of them a top-level directory holds: what the emptying of a folder keeps.

use std::ffi::{OsStr, OsString};
use std::os::fd::OwnedFd;

use super::Os;

/// What macOS's own per-user daemons make in a slot's per-user folder, as (top-level
/// directory, name), that root may not remove: Launch Services' data vault (`lsd`), which root
/// may not even examine ([`Os::sealed`]), and the trust daemon's folder (`trustd`, made when a
/// job checks a TLS certificate), which carries the system's no-unlink flag ([`Os::pinned`]),
/// as the folder's own directories do. A sealed one is kept whole: no job can open it, only
/// the daemon. A pinned one is kept and emptied, as a top-level directory is. One of these that
/// is neither is removed like anything else, and any other item root is refused holds the
/// account.
pub(super) const SYSTEM_KEPT: [(&str, &str); 2] = [
    ("0", "com.apple.LaunchServices.dv"),
    ("T", "com.apple.trustd"),
];

/// The entries of [`SYSTEM_KEPT`] in a top-level directory of a per-user folder that stay.
#[derive(Default)]
pub(super) struct Kept {
    /// Sealed: kept whole, never looked into.
    pub(super) whole: Vec<OsString>,
    /// Pinned: kept, and what is in them goes.
    pub(super) emptied: Vec<OsString>,
}

impl Kept {
    /// Whether the entry `name` stays.
    pub(super) fn holds(&self, name: &OsStr) -> bool {
        self.whole
            .iter()
            .chain(&self.emptied)
            .any(|kept| kept == name)
    }
}

/// The entries of the top-level directory `top` (named `top_name`) of a per-user folder that
/// stay: those of [`SYSTEM_KEPT`] for it that are there and sealed or pinned.
pub(super) fn kept_in(top: &OwnedFd, top_name: &OsStr, os: &dyn Os) -> Kept {
    let mut kept = Kept::default();
    for (_, name) in SYSTEM_KEPT
        .iter()
        .filter(|(dir, _)| OsStr::new(dir) == top_name)
    {
        let name = OsString::from(name);
        if os.sealed(top, &name) {
            kept.whole.push(name);
        } else if os.pinned(top, &name) {
            kept.emptied.push(name);
        }
    }
    kept
}
