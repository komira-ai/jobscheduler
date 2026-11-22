//! The runtime of this build, chosen when it is compiled: a Linux build runs its jobs with
//! `crun` (`js-contain`), or, opted in with `--allow-container-engine`, through the container
//! engine at `--container-bin`; a macOS build runs native jobs as accounts of its own pool,
//! started by its supervisor ([`crate::native`]), with the VM backend beside them ([`crate::vm`]). Only
//! this module is compiled per platform: `linux.rs` or `macos.rs` defines the flags of the
//! build's runtime ([`Flags`], flattened into [`crate::Cli`]), the checks the agent makes of
//! them at start, the [`Backend`] the worker runs jobs with, and what the hello says of it.
//! A flag of the other platform does not exist in this build: the agent refuses it by name
//! ([`crate::cli::removed_flag`]), with the other platform's flag names below.
//!
//! No other platform builds the agent.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::*;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::*;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("js-agent builds for Linux (the crun runtime) and macOS (the native runtime) only");

/// The flags only a Linux build has (its [`Flags`] there), without the leading `--`. A macOS
/// build refuses each by name; a test of the Linux build holds the list to the flags clap
/// knows there.
pub const LINUX_ONLY: [&str; 32] = [
    "allow-host-jobs",
    "allow-fuse-jobs",
    "watch-dir",
    "container-bin",
    "allow-container-engine",
    "crun-bin",
    "image-budget-gib",
    "cpu-ceiling",
    "job-user",
    "gpu-device",
    "gpu-mount",
    "gpu-env",
    "container-user",
    "container-subuid",
    "container-host",
    "container-pull",
    "container-tmp-size",
    "container-add-host",
    "egress",
    "egress-allow",
    "egress-dns",
    "egress-forward-port",
    "host-port",
    "egress-pool",
    "egress-ip",
    "egress-nft",
    "egress-nsenter",
    "actions-cgroup",
    "gpu-toolkit-hook",
    "image-allow-loopback",
    "settle-ms",
    "taint-recheck-ms",
];

/// The flags only a macOS build has (its [`Flags`] there), without the leading `--`. A Linux
/// build refuses each by name; a test of the macOS build holds the list to the flags clap
/// knows there.
pub const MACOS_ONLY: [&str; 18] = [
    "native-accounts",
    "native-account-range",
    "native-account-ready",
    "native-homes",
    "native-network",
    "native-deny-read",
    "native-max-file-mib",
    "native-max-processes",
    "native-tool-root",
    "native-pf-uid-start",
    "native-pf-uid-count",
    "native-pf-endpoint",
    "native-pf-anchor",
    "native-pf-state-dir",
    "native-pfctl",
    "native-trees-root",
    "vm-driver",
    "vm-store",
];
