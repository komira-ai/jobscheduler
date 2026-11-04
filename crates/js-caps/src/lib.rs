//! Worker capabilities: parsers over the text a node's OS publishes about its CPU
//! (Linux `/proc/cpuinfo`, macOS `sysctl hw.optional`) and GPUs (Linux PCI functions
//! in sysfs), the ISA levels that text
//! implies, and matching of an action's capability request against a node.
//!
//! - [`CpuCaps`] holds a node's architecture ([`Arch`]: `x86_64` or `arm64`, with
//!   `aarch64` read as `arm64`), every feature it reports (kernel names) and its highest
//!   ISA level: an x86-64 psABI level ([`X86Level`]) or an Armv8 version
//!   ([`ArmVersion`]). These types are `js-types`' own, re-exported.
//! - [`gpus_from_linux_pci`] and [`gpus_from_macos_sysctl`] count a node's GPUs, the
//!   `gpu` capacity a request books whole and exclusively; [`gpu_cores_from_macos_ioreg`]
//!   and [`metal_family_of_apple_chip`] describe a Mac's.
//! - [`Request`] and [`NodeCaps`] match with typed comparisons: exact, at least for
//!   ordered levels, subset for feature sets, membership for sets a node reports
//!   (`xcode`, and `vm.image` on its digest), minimums for countable resources, and one
//!   iOS device that satisfies every device attribute (`ios.device=1`).
//! - [`Request::from_platform`] reads an action's REAPI platform (`OSFamily`, `ISA`
//!   and kbf's own keys, the names in any case: [`property_name`]) as a request;
//!   [`NodeCaps::from_report`] reads a daemon's node report as the capabilities
//!   requests are matched against.
//! - [`HostContract`] is what a pool requires of the capabilities a host reports about how
//!   it runs jobs ([`RUNTIME`], [`GPU_TOOLKIT`], [`EXEC`]).
//!
//! This is a pure crate: no async runtime, network, clock, randomness or hashed
//! collections. The lists in `clippy.toml` enforce it.

#![deny(clippy::disallowed_methods, clippy::disallowed_types)]
// Moved unchanged from a workspace that does not set `missing_docs`: enum variants,
// struct fields and one method are undocumented. Remove this once they are documented.
#![allow(missing_docs)]

mod cpu;
mod gpu;
mod host;
mod macos;
mod matching;
mod platform;
mod report;

pub use cpu::{CpuCaps, ParseError};
pub use gpu::{
    GPU_KIND_NATIVE, PciFunction, gpu_cores_from_macos_ioreg, gpus_from_linux_pci,
    gpus_from_macos_sysctl, metal_family_of_apple_chip,
};
pub use host::{
    BUNDLE, BUNDLE_ROOT, Breach, DETECTED_KEYS, DISK_GIB, EXEC, FLAG_SET, GPU_TOOLKIT,
    HostContract, RUN_FILES, RUNTIME, SCRATCH_DEFAULT_GIB, SCRATCH_QUOTA, SCRATCH_QUOTA_PROJECT,
};
pub use matching::{
    Consumable, DEVICE_ATTRIBUTES, NodeCaps, REPORT_ONLY_KEYS, RESERVED_KEYS, Request,
    RequestError, Unmet,
};
pub use platform::{DAEMON_OSES, FromPlatformError, REAPI_KEYS, property_name};
pub use report::ReportError;
// The architecture and the ISA levels are defined in `js-types`, where a job's
// requirements and an offer name them too; they are re-exported here unchanged.
pub use js_types::{Arch, ArmVersion, IsaLevel, UnknownArch, UnknownIsaLevel, X86Level};
