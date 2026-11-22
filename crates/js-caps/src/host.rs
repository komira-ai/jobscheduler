//! The host contract: the capabilities an agent reports about what its host can run
//! jobs with, and the check a pool makes of them at admission.
//!
//! An agent reports these keys in its hello, from what it actually finds, and an
//! operator cannot set them ([`DETECTED_KEYS`]):
//!
//! | Key | Value | Reported when |
//! |---|---|---|
//! | [`RUNTIME`] | an [`ExecMode`] name: `container` or `sandbox-exec` | the program the agent runs jobs with was found and ran |
//! | [`GPU_TOOLKIT`] | [`FLAG_SET`] | the host offers GPUs and the toolkit check passed |
//! | [`GPU_PASSTHROUGH`] | [`FLAG_SET`] | the crun runtime gives a job the device nodes of the GPUs it booked |
//! | [`EXEC`] | an [`ExecMode`] name | the agent runs jobs, in that mode |
//! | [`DISK_GIB`] | whole GiB | the agent has a work volume (`--scratch-root`) and could read its free space |
//! | [`RUN_FILES`] | [`FLAG_SET`] | the agent runs containers and writes a start's run files |
//! | [`SCRATCH_QUOTA`] | [`SCRATCH_QUOTA_PROJECT`] | the agent enforces a quota on each run's scratch disk |
//! | [`SCRATCH_DEFAULT_GIB`] | whole GiB | the quota it enforces on a run that names none |
//! | [`BUNDLE`] | the held bundle manifest digests, `sha256:<hex>`, comma-separated | the agent has a bundle store and holds at least one bundle |
//! | [`BUNDLE_ROOT`] | the absolute path of the bundle store | the agent has a bundle store |
//! | [`LAYERED_ROOTFS`] | [`FLAG_SET`] | the agent reads a job start's layers, mounts and host ports, and its runtime and kernel can make them (or refuse by name what they cannot) |
//! | [`FULL_DISK_ACCESS`] | `granted`, `not-granted` or `undetermined` | the agent runs native jobs on a Mac and asked its root helper whether it holds Full Disk Access; only `granted` runs native jobs |
//! | [`FUSE_JOBS`] | [`FLAG_SET`] | the agent runs contained jobs that ask for FUSE (`--allow-fuse-jobs`), its kernel is at least 6.14, and `/dev/fuse` is the kernel's FUSE device (char 10:229) |
//! | [`FUSE_URING`] | [`FLAG_SET`] | as [`FUSE_JOBS`], and the fuse module's `enable_uring` is on and `kernel.io_uring_disabled` is 0, so FUSE over io_uring is usable; reported only together with [`FUSE_JOBS`] |
//! | [`OS_VERSION`] | the major version of the host's operating system, a decimal number such as `26` | the agent runs on macOS and could read the version |
//! | [`XCODE`] | the builds of the Xcodes installed in `/Applications` that answered the agent's survey, ascending, comma-separated | the agent runs on macOS and at least one Xcode is ready |
//! | `mount-provider.<name>` ([`mount_provider_key`]) | [`FLAG_SET`] | the agent reports layered rootfs and was given the node-local provider `<name>` (`--mount-provider`) |
//!
//! A job request may require [`EXEC`] (the same mode), [`DISK_GIB`] (at least that
//! much free disk), [`LAYERED_ROOTFS`], [`FUSE_JOBS`], [`FUSE_URING`], a `mount-provider.<name>` key (the same
//! value), [`XCODE`] (one build the host has) and [`OS_VERSION`] (at least that major
//! version); see [`crate::Request`]. The provider keys are a family, not names, so they are
//! not among [`DETECTED_KEYS`]: [`is_mount_provider_key`] says whether a key is one.
//!
//! A pool that declares an execution mode makes a [`HostContract`] of it: `runtime` must
//! equal the mode, and a pool with GPUs also needs `gpu-toolkit`. The check is
//! [`HostContract::check`]; it answers with the [`Breach`] that decides which refusal
//! the registry sends. Keys missing from a report are never filled in: an agent that
//! predates the contract reports none of them.

use std::collections::BTreeMap;

use js_types::ExecMode;

/// The capability key of the program the agent runs jobs with.
pub const RUNTIME: &str = "runtime";

/// The capability key of a working GPU container toolkit.
pub const GPU_TOOLKIT: &str = "gpu-toolkit";

/// The capability key of the mode the agent runs jobs in.
pub const EXEC: &str = js_types::EXEC_KEY;

/// The capability key of the major version of the host's operating system (`26`: macOS 26).
/// A job names the least version it needs under it, and a host is capable when it reports
/// that major version or a later one.
pub const OS_VERSION: &str = js_types::OS_VERSION_KEY;

/// The capability key of the ready Xcodes a Mac's agent found: their builds (`16C5032a`, the
/// `ProductBuildVersion` of each `Xcode*.app` that answered the agent's survey), ascending
/// and comma-separated in the one value a hello holds per key. A job names one build under
/// it, and a host is capable when
/// that build is among its builds (membership, not equality).
pub const XCODE: &str = "xcode";

/// The capability key of whether the Mac's root helper `js-mac-session` holds Full Disk Access
/// (the privacy service `SystemPolicyAllFiles`), which a native job needs of the host: the
/// agent asks the helper at start. Its value is `granted`, `not-granted` or `undetermined`
/// (the helper could not tell, or is older than the question); an agent whose value is not
/// `granted` refuses every native job (`no-full-disk-access`). A host's listing shows the value
/// as reported. An operator cannot set it with a flag, and a request cannot name it.
pub const FULL_DISK_ACCESS: &str = "full-disk-access";

/// The capability key of the free disk, in whole GiB, of the volume jobs work on.
pub const DISK_GIB: &str = js_types::DISK_GIB_KEY;

/// The capability key of an agent that writes a start's run files (`JobSpec::files`) and
/// mounts them at `/run/js-files`. An agent without it ignores the field, so the server
/// sends a start with files only to a host that reports it as [`FLAG_SET`].
pub const RUN_FILES: &str = "run-files";

/// The capability key of an agent that bounds a run's scratch disk with a project quota
/// and refuses a run it has no room for. Its value is [`SCRATCH_QUOTA_PROJECT`]. An agent
/// without it runs a job's `disk_bytes` unbounded, so the server sends a job that sets it
/// only to a host that reports this key, and a pool that names it in its capabilities
/// must book disk (`disk_bytes` above zero in its shape).
pub const SCRATCH_QUOTA: &str = "scratch-quota";

/// The value of [`SCRATCH_QUOTA`] for a quota enforced by an XFS project quota.
pub const SCRATCH_QUOTA_PROJECT: &str = "project";

/// The capability key of the quota, in whole GiB, the agent enforces on a run that names
/// no `disk_bytes`. Reported with [`SCRATCH_QUOTA`]; the server books it for such a run.
pub const SCRATCH_DEFAULT_GIB: &str = "scratch-default-gib";

/// The capability key of the bundles a host holds: the manifest digests (`sha256:<hex>`),
/// comma-separated, in ascending order. A job that names a bundle goes to a host that holds
/// it, because a host does not fetch one by default.
pub const BUNDLE: &str = "bundle";

/// The capability key of the bundle store's root path, a fact: tools embed it in their
/// output, so a fleet uses one path and a host with another is not eligible.
pub const BUNDLE_ROOT: &str = "bundle-root";

/// The capability key of an agent whose crun runtime gives a job the device nodes, driver
/// mounts and variables of the GPUs it booked (`--gpu-device`). Its value is [`FLAG_SET`].
/// It tells the operator so; a GPU job is placed by the GPUs a host reports, and no pool or
/// request reads this key.
pub const GPU_PASSTHROUGH: &str = "gpu-passthrough";

/// The capability key of an agent that reads a job start's `layers`, `mounts` and
/// `host_ports` (`js_agent_wire::JobStartSpec`) and refuses by name what its runtime cannot
/// make of them. Its value is [`FLAG_SET`]. An agent without it ignores the three fields and
/// would run the job on its image alone, so a job that sets any of them is placed only on a
/// host that reports this key, and its start is sent only there. A request compares it
/// exactly.
pub const LAYERED_ROOTFS: &str = "layered-rootfs";

/// The capability key of an agent that gives a contained job the FUSE device (`--allow-fuse-jobs`):
/// `/dev/fuse` and `CAP_SYS_ADMIN` of the container's own user namespace. Its value is
/// [`FLAG_SET`]. The agent reports it only when its kernel is at least 6.14 and `/dev/fuse` is
/// the kernel's FUSE device (char 10:229). An agent without it runs a job that asks for FUSE
/// without the device, so such a job is placed only on a host that reports this key, and its
/// start is sent only there. An operator cannot set it with a flag; a request compares it
/// exactly.
pub const FUSE_JOBS: &str = "fuse-jobs";

/// The capability key of a host where FUSE over io_uring is usable: the agent reports
/// [`FUSE_JOBS`], the fuse module's `enable_uring` is on and `kernel.io_uring_disabled` is 0.
/// Its value is [`FLAG_SET`]. It is reported only together with [`FUSE_JOBS`]. An operator
/// cannot set it with a flag; a request compares it exactly.
pub const FUSE_URING: &str = "fuse-uring";

/// The value of a capability that is a flag: present means set.
pub const FLAG_SET: &str = "true";

/// The prefix of the capability key of a node-local mount provider an agent was given:
/// `mount-provider.<name>` ([`mount_provider_key`]). Its value is [`FLAG_SET`]. A job
/// that names the provider `<name>` for a layer or a mount requires the key, so it is
/// placed only on a host that reports it, and its start is sent only there. An operator
/// cannot set such a key with a flag; a request compares it exactly.
pub const MOUNT_PROVIDER_PREFIX: &str = "mount-provider.";

/// The capability key of the mount provider `name`: [`MOUNT_PROVIDER_PREFIX`] and the name.
#[must_use]
pub fn mount_provider_key(name: &str) -> String {
    format!("{MOUNT_PROVIDER_PREFIX}{name}")
}

/// Whether `key` is a mount provider's key: [`MOUNT_PROVIDER_PREFIX`] and a name of at
/// least one byte.
#[must_use]
pub fn is_mount_provider_key(key: &str) -> bool {
    key.strip_prefix(MOUNT_PROVIDER_PREFIX)
        .is_some_and(|name| !name.is_empty())
}

/// The keys an agent detects and an operator cannot set with a flag.
pub const DETECTED_KEYS: [&str; 16] = [
    RUNTIME,
    GPU_TOOLKIT,
    GPU_PASSTHROUGH,
    EXEC,
    DISK_GIB,
    RUN_FILES,
    SCRATCH_QUOTA,
    SCRATCH_DEFAULT_GIB,
    BUNDLE,
    BUNDLE_ROOT,
    LAYERED_ROOTFS,
    FUSE_JOBS,
    FUSE_URING,
    OS_VERSION,
    XCODE,
    FULL_DISK_ACCESS,
];

/// How a report fails a [`HostContract`]. Each names the capability at fault.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Breach {
    /// A capability the contract needs is not in the report (or, for the toolkit, is not
    /// set): the host has no runtime, or no working toolkit.
    #[error("the host contract needs the capability {field}")]
    Missing {
        /// The capability key.
        field: &'static str,
    },
    /// A capability names a mode other than the pool's.
    #[error("the capability {field} names another execution mode than the pool's")]
    Mismatch {
        /// The capability key.
        field: &'static str,
    },
}

impl Breach {
    /// The capability key at fault.
    #[must_use]
    pub fn field(self) -> &'static str {
        match self {
            Self::Missing { field } | Self::Mismatch { field } => field,
        }
    }
}

/// What a pool requires of the hosts that join it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostContract {
    exec: ExecMode,
    gpus: bool,
}

impl HostContract {
    /// The contract of a pool whose hosts run jobs in `exec`; `gpus` is whether the pool
    /// offers GPUs, which makes the toolkit part of the contract.
    #[must_use]
    pub fn new(exec: ExecMode, gpus: bool) -> Self {
        Self { exec, gpus }
    }

    /// Checks the capabilities a hello reported, in this order, and answers with the
    /// first breach:
    ///
    /// 1. `exec`, when reported, differs from the pool's mode: [`Breach::Mismatch`] on `exec`.
    /// 2. `runtime` is not reported: [`Breach::Missing`] on `runtime`.
    /// 3. `runtime` differs from the pool's mode: [`Breach::Mismatch`] on `runtime`.
    /// 4. The pool has GPUs and `gpu-toolkit` is not [`FLAG_SET`]: [`Breach::Missing`] on
    ///    `gpu-toolkit`.
    ///
    /// # Errors
    ///
    /// The first [`Breach`] of the list above.
    pub fn check(&self, reported: &BTreeMap<String, String>) -> Result<(), Breach> {
        let mode = self.exec.name();
        let differs = |key: &str| reported.get(key).is_some_and(|value| value != mode);
        if differs(EXEC) {
            return Err(Breach::Mismatch { field: EXEC });
        }
        if !reported.contains_key(RUNTIME) {
            return Err(Breach::Missing { field: RUNTIME });
        }
        if differs(RUNTIME) {
            return Err(Breach::Mismatch { field: RUNTIME });
        }
        let toolkit = reported.get(GPU_TOOLKIT).map(String::as_str) == Some(FLAG_SET);
        if self.gpus && !toolkit {
            return Err(Breach::Missing { field: GPU_TOOLKIT });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    /// Asserts each step of the check on its own, so the order of the list in
    /// [`HostContract::check`] is the order that decides: a report that breaks two steps
    /// gets the first one's breach.
    #[test]
    fn the_first_breach_decides() {
        let pool = HostContract::new(ExecMode::Container, true);
        let whole = [
            (RUNTIME, "container"),
            (EXEC, "container"),
            (GPU_TOOLKIT, "true"),
        ];
        assert_eq!(pool.check(&report(&whole)), Ok(()));

        let none = report(&[]);
        assert_eq!(pool.check(&none), Err(Breach::Missing { field: RUNTIME }));

        let other_exec = report(&[(RUNTIME, "container"), (EXEC, "sandbox-exec")]);
        let breach = Breach::Mismatch { field: EXEC };
        assert_eq!(pool.check(&other_exec), Err(breach));
        assert_eq!(breach.field(), "exec");

        let other_runtime = report(&[(RUNTIME, "sandbox-exec"), (GPU_TOOLKIT, "true")]);
        let breach = Breach::Mismatch { field: RUNTIME };
        assert_eq!(pool.check(&other_runtime), Err(breach));
        assert_eq!(breach.field(), "runtime");

        let no_toolkit = report(&[(RUNTIME, "container"), (EXEC, "container")]);
        let breach = Breach::Missing { field: GPU_TOOLKIT };
        assert_eq!(pool.check(&no_toolkit), Err(breach));
        assert!(breach.to_string().contains("gpu-toolkit"));
        assert_eq!(breach.field(), "gpu-toolkit");
        let said = Breach::Mismatch { field: RUNTIME }.to_string();
        assert!(said.contains("another execution mode"), "{said}");
        let unset = report(&[(RUNTIME, "container"), (GPU_TOOLKIT, "false")]);
        assert_eq!(pool.check(&unset), Err(breach));
    }

    /// Asserts that a pool without GPUs does not ask for the toolkit, and that a report
    /// with no `exec` passes when `runtime` is right (an agent need not name both).
    #[test]
    fn a_pool_without_gpus_needs_no_toolkit() {
        let pool = HostContract::new(ExecMode::SandboxExec, false);
        let mac = report(&[(RUNTIME, "sandbox-exec")]);
        assert_eq!(pool.check(&mac), Ok(()));
        let container = report(&[(RUNTIME, "container")]);
        let breach = Breach::Mismatch { field: RUNTIME };
        assert_eq!(pool.check(&container), Err(breach));
    }
}
