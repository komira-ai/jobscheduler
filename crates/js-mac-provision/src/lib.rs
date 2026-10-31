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
//! - [`launchd`]: the property list of the LaunchDaemon that keeps the anchor loaded.
//! - [`daemon`]: the property lists of the js-agent package's two LaunchDaemons.
//! - [`postinstall`]: what the package's postinstall does, over a `launchctl` trait.
//! - [`probe()`]: one connect and its outcome, for the operator and the root test.
//! - [`cli`]: the verbs of the `js-mac-provision` program.
//!
//! Everything builds and is tested on every platform with a fake `pfctl`; only the real
//! `pfctl` needs a Mac and root.

pub mod cli;
pub mod daemon;
pub mod launchd;
pub mod pfctl;
pub mod policy;
pub mod postinstall;
pub mod probe;
pub mod provision;
pub mod render;

#[cfg(test)]
mod testing;

pub use policy::{AnchorName, Endpoint, PolicyError, UidRange};
pub use probe::probe;
pub use provision::{Config, enforced};
pub use render::render;
