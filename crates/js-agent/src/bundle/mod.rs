//! Bundles on a Mac: the flags that configure the store, what the agent reports about it,
//! and the checks a start that names a bundle must pass.
//!
//! A bundle is a darwin toolchain a job pins by digest (`JobSpec::bundle`); the agent keeps
//! unpacked bundles in a store (`--bundle-store`). This module is the agent's side of the
//! policy around that store, over one seam, [`BundleHost`], so every rule is tested with a
//! fake host:
//!
//! - [`Settings`]: the `--bundle-*` and `--read-model` flags, checked together. A flag that
//!   cannot work refuses the start of the agent (exit status 2): a store off a Mac, off the
//!   scratch volume or on a volume that ignores ownership, trust `list` with no list or an
//!   empty one, trust `digest` with `--bundle-pull missing` (any submitter could make the
//!   host download and unpack), a registry entry that is a bare host.
//! - [`Admission`]: the checks of a start, in order: the deny list, the allow list, the
//!   registry the name points to, whether the store holds the bundle (or a fetch may begin),
//!   the macOS it needs, the Xcode it needs. A refusal carries one of the codes of
//!   [`js_agent_wire::refusal`], and [`js_agent_wire::refusal::bundle_class`] says whose
//!   fault it is.
//! - [`report`]: what the hello says (`bundle`, `bundle-root`) and the [`js_agent_wire::Caps`]
//!   frame, and [`reserve_bytes`]: the part of the budget the store has not used, which the
//!   disk offered to jobs leaves out.
//!
//! Nothing here runs a bundle. A start that passes every check ends at the lifecycle's
//! refusal that this agent cannot run one yet.

mod admit;
mod host;
mod list;
mod place;
mod report;
mod settings;

pub use admit::Admission;
pub use host::{BundleHost, Fetch, HostXcode, MAX_LIST_BYTES, Needs, SystemHost};
pub use list::{ListError, parse_digests};
pub use place::{PlaceError, Pinned, Placer, load_config};
pub use report::{Reported, hello_capabilities, report, reserve_bytes};
pub use settings::{Pull, Registry, Settings, Trust};
