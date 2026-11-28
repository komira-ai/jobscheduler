//! `js-mac-provision`: limits the egress of a Mac pool's users with a pf anchor.
//!
//! Under `Net::On` a Seatbelt profile cannot name the hosts a process may reach (its
//! `remote ip` filter takes only `*` and `localhost`), so the network of a pool user is
//! limited outside the sandbox, by the packet filter, keyed on the user's uid. See the
//! README for the design, the persistence choice and what the anchor does not stop.
//!
//! - [`policy`]: the uid range, the endpoints and the anchor name, checked when made.
//! - [`render()`]: the anchor's text, a pure function of the policy.
//! - [`pfctl`]: `pfctl` behind a trait, with the real runner.
//! - [`provision`]: install, check and remove over a [`pfctl::Pfctl`].
//! - [`counters`]: the per-rule packet counters of the anchor, which show pf blocked a packet.
//! - [`agent_flags`]: the anchor the macOS agent's flags file names.
//! - [`apply()`]: loads that anchor unless it is in force; `js-agent __pf-anchor` runs it.
//! - [`launchd`]: the property list of the LaunchDaemon that keeps the anchor loaded.
//! - [`daemon`]: the property lists of the js-agent package's two LaunchDaemons.
//! - [`probe()`]: one connect and its outcome, for the root test.
//!
//! Everything builds and is tested on every platform with a fake `pfctl`; only the real
//! `pfctl` needs a Mac and root.

pub mod agent_flags;
pub mod apply;
pub mod counters;
pub mod daemon;
pub mod launchd;
pub mod pfctl;
pub mod policy;
pub mod probe;
pub mod provision;
pub mod render;

#[cfg(test)]
mod testing;

pub use apply::{Applied, ApplyError, apply};
pub use policy::{AnchorName, Endpoint, PolicyError, UidRange};
pub use probe::probe;
pub use provision::{Config, enforced};
pub use render::render;
