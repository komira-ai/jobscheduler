//! The Seatbelt profile of one run, generated from a typed [`Policy`].
//!
//! The model: `(allow default)`, then deny writes, network and a few classes, grant back
//! narrowly, and end with a floor. A later rule wins, so the floor beats every grant.
//! Reads and exec are open by default ([`ReadModel::Open`]) because toolchains read
//! widely; the profile is a hermeticity layer and defence in depth, and the boundary is
//! the user the run executes as.
//!
//! What this module guarantees:
//!
//! - Every path is a `-D` parameter named by the builder and used as `(param "NAME")`.
//!   No path text, and nothing a caller chose, is part of the profile text, so there is
//!   no quoting surface. A path is also checked ([`AbsPath`]) so a bad one is refused
//!   early and the parameter list is a sane command line.
//! - A caller cannot build a [`Policy`] except from a [`SlotLayout`] and the agent's
//!   [`Caps`], and cannot change it except with a [`Narrowing`], which only lowers the
//!   network, drops a grant or adds a deny.
//! - The floor is a private constant emitted after every other rule.
//!
//! The allow-list read model ([`ReadModel::Allow`]) differs in the read section only:
//!
//! - Reads are denied, then granted: the base set of one macOS major version
//!   ([`MacosMajor`]; a version without a golden has no profile), the run's own places,
//!   and the [`ReadGrant`]s of [`Policy::allowing`]. A grant is made of a proof object
//!   (a [`PinnedTree`], a ready [`HostXcode`](crate::xcode::HostXcode), a kind of the
//!   closed vocabulary), so a caller cannot name a path, and no directory-wide grant
//!   exists ([`is_directory_wide`]).
//! - [`Policy::allowing`] takes an [`Assurance`], which only a passing start-up
//!   [`SelfTest`] on the running host makes: the profile is the security property under
//!   this model and is only as right as it is for the macOS build that runs it.
//! - A program of a bundle is a [`ToolPath`], made from an entrypoint of a pinned tree.
//!
//! What it does not do:
//!
//! - `(deny process-info* (target others))` is in the profile as defence in depth but
//!   does not hide processes: on a hosted runner `pgrep` listed the same processes with
//!   and without it. Hiding another user's processes is the user boundary's job.
//! - Loopback is shared by every user of the Mac; see [`Net`].
//! - The profile compiler rejects `localhost:LOW-HIGH`, so per-port allows are
//!   single `localhost:PORT` entries ([`Ports`]).
//! - Under [`Net::On`] no address can be denied: the profile compiler takes only `*` and
//!   `localhost` as the host of a `remote ip` filter (a runner test pins this). Blocking
//!   the scheduler or the cloud metadata address needs a packet filter keyed on the
//!   pool's user range (tracked in komira-ai/jobscheduler#407).
//! - Every read-denied path is also write-denied and its unix sockets are neither
//!   connectable nor bindable, under every network level. The builder keeps such paths
//!   and denies them, rather than refusing a read-denied path inside the run directory,
//!   because a layout may keep a read-denied state directory under a granted one.
//! - A path is only spelled correctly by [`AbsPath`]; its canonical form (symbolic
//!   links, Unicode normalisation) is the caller's duty, and the host step pins it.
//! - It starts nothing; applying the profile is the caller's `exec` of
//!   [`Profile::argv`].

mod allow;
mod grant;
mod net;
mod path;
mod policy;
mod profile;
mod selftest;
mod tool;

pub use allow::{BaseSet, MacosMajor};
pub use grant::{Match, PinnedTree, ReadGrant, is_directory_wide, kind_rules};
pub use net::{MAX_PORTS, Net, Ports, PortsError};
pub use path::{AbsPath, MAX_PATH_BYTES, PathError};
pub use policy::{Caps, MAX_LISTED, Narrowing, Policy, PolicyError, ReadModel, SlotLayout};
pub use profile::{BuildError, Profile, SANDBOX_EXEC};
pub use selftest::{
    Assurance, Canaries, Class, Failure, Fixture, Role, SETUID_CANARY, STUB_OUTPUT, STUB_SOURCE,
    SelfTest, SelfTestError, Step, Why,
};
pub use tool::{BUNDLE_PREFIX, ToolError, ToolPath, check_path_dirs};
