//! `config.json` for a host job's container ([`Isolation::Host`]): built by
//! [`action_config`](super::spec::action_config) from the same [`ContainerSpec`] as a
//! contained job's, with the namespaces, the root and the privileges the only differences.
//!
//! | Setting                 | Contained job                                   | Host job                                              |
//! |-------------------------|-------------------------------------------------|-------------------------------------------------------|
//! | `linux.namespaces`      | new pid, network, IPC, UTS, user, cgroup, mount | none: the host's own                                  |
//! | id mappings             | onto the job user's subordinate range           | none (no user namespace)                              |
//! | `root`                  | the image's unpacked root, read-only            | the host's `/`, as it is mounted                      |
//! | `mounts`                | `/proc`, `/dev`, the overlay, the binds, ...    | none: a mount would change the host's mount namespace |
//! | `process.user`          | 1000:1000 in the user namespace                 | 0:0, the host's root                                  |
//! | capabilities            | none                                            | every capability                                      |
//! | seccomp, masked paths   | the pinned profile and lists                    | none                                                  |
//! | `noNewPrivileges`       | true                                            | false (a host job may run a set-uid program)          |
//! | `hostname`              | `localhost`                                     | none: setting it would rename the host                |
//! | `process.cwd`           | the exec root or below it                       | `exec_root`: the run's scratch directory on the host  |
//! | `process.args`          | the init at its bind in `/dev`, then the argv   | the init at its path on the host, then the argv       |
//! | `cgroupsPath`           | `<lease cgroup>/ctr`                            | the same                                              |
//! | pids limit, OOM group   | `resources.pids`, `memory.oom.group=1`          | the same                                              |
//! | rlimits, OOM score      | `nofile`, `nproc`, no core dump; the default     | the same                                              |
//!
//! The memory and CPU limits stay on the lease cgroup, as for a contained job. A host spec
//! with a mount, a shared tree or a device is refused: the job sees the host's own paths
//! and devices, and none of them could be given without changing the host.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use oci_spec::runtime::{
    Capability, LinuxBuilder, LinuxCapabilitiesBuilder, LinuxPidsBuilder, LinuxResourcesBuilder,
    PosixRlimitType, ProcessBuilder, RootBuilder, Spec, SpecBuilder, UserBuilder,
};

use super::hardening::capability;
use super::spec::{OCI_VERSION, Placement, SpecError, built, normalized, rlimit};
use crate::container::ContainerSpec;

/// Every capability `oci-spec` names: a host job holds them all, as root on the host does.
pub(crate) const HOST_CAPABILITIES: [&str; 41] = [
    "CAP_AUDIT_CONTROL",
    "CAP_AUDIT_READ",
    "CAP_AUDIT_WRITE",
    "CAP_BLOCK_SUSPEND",
    "CAP_BPF",
    "CAP_CHECKPOINT_RESTORE",
    "CAP_CHOWN",
    "CAP_DAC_OVERRIDE",
    "CAP_DAC_READ_SEARCH",
    "CAP_FOWNER",
    "CAP_FSETID",
    "CAP_IPC_LOCK",
    "CAP_IPC_OWNER",
    "CAP_KILL",
    "CAP_LEASE",
    "CAP_LINUX_IMMUTABLE",
    "CAP_MAC_ADMIN",
    "CAP_MAC_OVERRIDE",
    "CAP_MKNOD",
    "CAP_NET_ADMIN",
    "CAP_NET_BIND_SERVICE",
    "CAP_NET_BROADCAST",
    "CAP_NET_RAW",
    "CAP_PERFMON",
    "CAP_SETFCAP",
    "CAP_SETGID",
    "CAP_SETPCAP",
    "CAP_SETUID",
    "CAP_SYSLOG",
    "CAP_SYS_ADMIN",
    "CAP_SYS_BOOT",
    "CAP_SYS_CHROOT",
    "CAP_SYS_MODULE",
    "CAP_SYS_NICE",
    "CAP_SYS_PACCT",
    "CAP_SYS_PTRACE",
    "CAP_SYS_RAWIO",
    "CAP_SYS_RESOURCE",
    "CAP_SYS_TIME",
    "CAP_SYS_TTY_CONFIG",
    "CAP_WAKE_ALARM",
];

/// [`HOST_CAPABILITIES`] as the typed set `oci-spec` takes.
fn every_capability() -> HashSet<Capability> {
    HOST_CAPABILITIES
        .iter()
        .filter_map(|name| capability(name))
        .collect()
}

/// The process environment of a host job: the Command's variables, then `HOME=/root`
/// unless it sets one. No `HOSTNAME`: the job sees the host's own name.
fn environment(env: &[(String, String)]) -> Vec<String> {
    let mut out: Vec<String> = env.iter().map(|(n, v)| format!("{n}={v}")).collect();
    if !env.iter().any(|(n, _)| n == "HOME") {
        out.push("HOME=/root".to_owned());
    }
    out
}

/// The container's `process.args`: the init at its path on the host (it reaps what the
/// job orphans, as it does in a contained job), `--`, then the job's argv. No route wait:
/// the host's network is already there.
fn init_argv(init: &str, argv: &[String]) -> Vec<String> {
    let mut args = vec![init.to_owned(), "--".to_owned()];
    args.extend(argv.iter().cloned());
    args
}

/// The `config.json` of the host container for `spec` (see the module's table).
///
/// # Errors
/// `spec.exec_root` or the init's path is not a normalized absolute path, or `spec` has a
/// mount, a shared tree or a device ([`SpecError::HostMounts`]).
pub(crate) fn host_config(
    spec: &ContainerSpec,
    placement: &Placement<'_>,
) -> Result<Spec, SpecError> {
    normalized("exec_root", &spec.exec_root, "/")?;
    let init = placement.init.to_string_lossy();
    normalized("init path", &init, "/")?;
    if !(spec.mounts.is_empty() && spec.shared.is_empty() && spec.devices.is_empty()) {
        return Err(SpecError::HostMounts);
    }
    let every = every_capability();
    let capabilities = built(
        LinuxCapabilitiesBuilder::default()
            .bounding(every.clone())
            .effective(every.clone())
            .permitted(every.clone())
            .inheritable(every.clone())
            .ambient(every)
            .build(),
    );
    let process = built(
        ProcessBuilder::default()
            .terminal(false)
            .user(built(UserBuilder::default().uid(0u32).gid(0u32).build()))
            .args(init_argv(&init, &spec.argv))
            .env(environment(&spec.env))
            .cwd(PathBuf::from(&spec.exec_root))
            .capabilities(capabilities)
            .rlimits(vec![
                rlimit(PosixRlimitType::RlimitNofile, spec.limits.nofile),
                rlimit(PosixRlimitType::RlimitNproc, spec.limits.nproc),
                rlimit(PosixRlimitType::RlimitCore, 0),
            ])
            .no_new_privileges(false)
            .oom_score_adj(0)
            .build(),
    );
    let pids = built(
        LinuxPidsBuilder::default()
            .limit(i64::try_from(spec.limits.pids).unwrap_or(i64::MAX))
            .build(),
    );
    let unified = HashMap::from([("memory.oom.group".to_owned(), "1".to_owned())]);
    let resources = LinuxResourcesBuilder::default().pids(pids).unified(unified);
    // Each list written out, empty: `oci-spec`'s defaults are a contained container's
    // namespaces, masked paths and read-only paths, which would mask and remount the
    // host's own `/proc` in a shared mount namespace.
    let linux = built(
        LinuxBuilder::default()
            .namespaces(Vec::new())
            .cgroups_path(PathBuf::from(format!("{}/ctr", placement.cgroup)))
            .resources(built(resources.build()))
            .masked_paths(Vec::<String>::new())
            .readonly_paths(Vec::<String>::new())
            .build(),
    );
    let root = built(RootBuilder::default().path("/").readonly(false).build());
    let mut config = built(
        SpecBuilder::default()
            .version(OCI_VERSION)
            .root(root)
            .process(process)
            .mounts(Vec::new())
            .linux(linux)
            .build(),
    );
    // `oci-spec` fills what a builder leaves out: a hostname would rename the host, and
    // annotations say nothing here.
    config.set_hostname(None);
    config.set_annotations(None);
    Ok(config)
}
